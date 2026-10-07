pub mod cache;
pub mod guard;
pub mod inventory;
pub mod policy;
pub mod process;
pub mod target;
pub mod timer;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asset_store::temp_workspace::{
    GcEntry, cleanup_legacy_orphans_under, cleanup_orphans_under, plan_legacy_orphans_under,
    plan_orphans_under,
};
use inventory::{OwnershipEvidence, TmpInventory, scan_tmp};
use policy::Policy;
use serde::{Serialize, Serializer};
use target::{CargoTargetMeasurement, CargoWorkspace, TargetBlocker, ThresholdDecision};

const REPORT_SCHEMA: u32 = 1;
// TempWorkspace uses `anki-decks-{uid}` as its private namespace directory.
const TEMP_NAMESPACE_PREFIX: &str = "anki-decks-";

pub(crate) fn serialize_report_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&redact_temp_namespace_uid(path).to_string_lossy())
}

pub(crate) fn serialize_optional_report_path<S>(
    path: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serializer.serialize_some(&redact_temp_namespace_uid(path).to_string_lossy()),
        None => serializer.serialize_none(),
    }
}

fn redact_temp_namespace_uid(path: &Path) -> PathBuf {
    let mut redacted = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if is_temp_namespace_with_uid(name) => {
                redacted.push("anki-decks-<uid>");
            }
            component => redacted.push(component.as_os_str()),
        }
    }
    redacted
}

fn is_temp_namespace_with_uid(component: &std::ffi::OsStr) -> bool {
    component
        .to_str()
        .and_then(|name| name.strip_prefix(TEMP_NAMESPACE_PREFIX))
        .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()))
}

fn display_report_path(path: &Path) -> String {
    redact_temp_namespace_uid(path).display().to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub category: &'static str,
    #[serde(serialize_with = "serialize_report_path")]
    pub path: PathBuf,
    pub ownership: String,
    pub bytes_before: u64,
    pub bytes_after: Option<u64>,
    pub bytes_freed: u64,
    pub reason: String,
    pub action: &'static str,
    pub result: &'static str,
    pub live: Option<bool>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemTmpfilesStatus {
    pub available: bool,
    pub timer_state: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub mode: &'static str,
    pub generated_unix_seconds: u64,
    #[serde(serialize_with = "serialize_report_path")]
    pub workspace_root: PathBuf,
    pub scan_elapsed_ms: u128,
    pub cleanup_elapsed_ms: u128,
    pub elapsed_ms: u128,
    pub external_cache_bytes_before: u64,
    pub external_cache_bytes_after: u64,
    pub external_cache_bytes_freed: u64,
    pub external_cache_unknown_entries: usize,
    pub cargo_target: Candidate,
    pub external_build_caches: Vec<cache::ExternalCacheCandidate>,
    pub project_temp: Vec<Candidate>,
    pub tmp: Vec<TmpInventory>,
    pub system_tmpfiles: SystemTmpfilesStatus,
    pub errors: usize,
    /// Ошибки операций обслуживания; ошибки частичной инвентаризации только для
    /// чтения сюда не входят.
    pub fatal_errors: usize,
    pub operation_errors: Vec<String>,
}

#[derive(Debug)]
pub struct RunOptions {
    pub apply: bool,
    pub detail: bool,
    pub mode: &'static str,
    pub temp_root: PathBuf,
    pub top_entries: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            apply: false,
            detail: false,
            mode: "scan",
            temp_root: PathBuf::from("/tmp"),
            top_entries: 20,
        }
    }
}

pub fn run(root: &Path, policy: &Policy, options: &RunOptions) -> Result<Report, String> {
    let started = Instant::now();
    let scan_started = Instant::now();
    let lock = if options.apply {
        match guard::MaintenanceLock::acquire() {
            Ok(lock) => Some(lock),
            Err(error) => {
                return Err(format!(
                    "очистка отложена: не удалось получить общую блокировку обслуживания: {error}"
                ));
            }
        }
    } else {
        None
    };
    let workspace = target::discover_workspace(root)?;
    let target_report = target::inspect_target(
        &workspace,
        policy.warning_bytes(),
        policy.hard_limit_bytes(),
    );
    let mut external_caches = match policy::cache_home() {
        Some(cache_home) => cache::inspect_external_caches(
            root,
            &cache_home.join("anki-decks/repository-maintenance"),
            policy.external_cache_min_age(),
        )
        .map_err(|error| {
            format!("не удалось инвентаризировать внешние каталоги сборки Cargo: {error}")
        })?,
        None => cache::ExternalCacheInventory::empty(),
    };
    let external_cache_bytes_before = external_caches.allocated_bytes;
    let external_cache_unknown_entries = external_caches.unknown_entries;
    let selected_external = if options.mode == "scan" {
        Vec::new()
    } else {
        cache::cleanup_plan(
            &mut external_caches,
            policy.external_hard_limit_bytes(),
            policy.external_goal_bytes(),
            policy.external_cache_min_age(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
    };
    let orphan_plan = plan_orphans_under(&options.temp_root, policy.orphan_min_age())
        .map_err(|error| format!("не удалось построить план очистки `TempWorkspace`: {error}"))?;
    let legacy_plan = plan_legacy_orphans_under(&options.temp_root, policy.orphan_min_age())
        .map_err(|error| format!("не удалось построить план legacy GC: {error}"))?;
    let mut orphan_entries = orphan_plan.entries;
    orphan_entries.extend(legacy_plan.entries);
    let temp_allocated_before = orphan_entries
        .iter()
        .map(|entry| {
            let allocated = inventory::measure_path(&entry.path)
                .map(|measurement| measurement.allocated_bytes)
                .unwrap_or(entry.bytes);
            (entry.path.clone(), allocated)
        })
        .collect::<BTreeMap<_, _>>();
    let mut temp_evidence = namespace_evidence(&orphan_entries, &options.temp_root);
    let mut temp_roots = vec![options.temp_root.clone()];
    let system_temp = std::env::temp_dir();
    if fs::canonicalize(&system_temp).ok() != fs::canonicalize(&options.temp_root).ok() {
        temp_roots.push(system_temp);
    }
    temp_roots.sort();
    temp_roots.dedup();
    let mut temp_before = Vec::new();
    for temp_root in &temp_roots {
        let evidence = if temp_root == &options.temp_root {
            temp_evidence.as_slice()
        } else {
            &[]
        };
        let top = if options.detail {
            usize::MAX
        } else {
            options.top_entries
        };
        temp_before.push(scan_tmp(temp_root, top, evidence).map_err(|error| {
            format!("не удалось просканировать {}: {error}", temp_root.display())
        })?);
    }
    let scan_elapsed_ms = scan_started.elapsed().as_millis();
    let mut cleanup_ms = 0_u128;
    let mut target_after = None;
    let mut target_freed = 0_u64;
    let mut target_result =
        if options.mode == "scan" && target_report.decision == ThresholdDecision::Clean {
            "threshold_exceeded"
        } else {
            "not_needed"
        };
    let mut target_action = match target_report.decision {
        ThresholdDecision::Keep => "keep",
        ThresholdDecision::Warn => "warn",
        ThresholdDecision::Clean if target_report.safe_to_clean && options.mode != "scan" => {
            "delete"
        }
        ThresholdDecision::Clean if target_report.safe_to_clean => "warn",
        ThresholdDecision::Clean => "deferred",
    };
    let mut target_error = None;
    match &target_report.blocker {
        Some(TargetBlocker::Busy) => {
            target_action = "deferred";
            target_result = "busy";
            target_error = Some(target_report.reason.clone());
        }
        Some(TargetBlocker::Unsafe(reason)) => {
            target_action = "error";
            target_result = "failed";
            target_error = Some(reason.clone());
        }
        None => (),
    }
    if target_report.decision == ThresholdDecision::Clean && target_report.safe_to_clean {
        let clean_start = Instant::now();
        if options.mode == "scan" {
            // `scan` сообщает состояние порогов, но не строит план действий.
        } else if options.apply {
            match clean_target_if_needed(
                &workspace,
                &target_report,
                policy.warning_bytes(),
                policy.hard_limit_bytes(),
            ) {
                TargetCleanOutcome::Removed => {
                    target_result = "removed";
                    target_after = Some(measure_workspace_bytes(&workspace));
                    target_freed = target_report
                        .allocated_bytes
                        .saturating_sub(target_after.unwrap_or(0));
                }
                TargetCleanOutcome::NotNeeded(decision) => {
                    target_action = match decision {
                        ThresholdDecision::Keep => "keep",
                        ThresholdDecision::Warn => "warn",
                        ThresholdDecision::Clean => unreachable!("порог обработки проверен"),
                    };
                    target_result = "not_needed";
                    target_error = None;
                }
                TargetCleanOutcome::Deferred(reason) => {
                    target_action = "deferred";
                    target_result = "busy";
                    target_error = Some(reason);
                }
                TargetCleanOutcome::Unsafe(error) => {
                    target_action = "error";
                    target_result = "failed";
                    target_error = Some(error);
                }
                TargetCleanOutcome::Failed(error) => {
                    target_action = "error";
                    target_result = "failed";
                    target_error = Some(error);
                }
            }
        } else {
            target_result = "planned";
        }
        cleanup_ms = cleanup_ms.saturating_add(clean_start.elapsed().as_millis());
    }
    if !selected_external.is_empty() {
        let clean_start = Instant::now();
        for id in selected_external {
            let Some(candidate) = external_caches
                .candidates
                .iter_mut()
                .find(|item| item.id == id)
            else {
                continue;
            };
            if options.apply {
                match recheck_external_cache(root, candidate) {
                    Ok(()) => match candidate
                        .target_dir
                        .as_deref()
                        .ok_or_else(|| "в маркере отсутствует `target_dir`".to_owned())
                        .and_then(|target_dir| cache::clean_cache(root, target_dir))
                    {
                        Ok(_) => {
                            let measured_after = candidate
                                .target_dir
                                .as_deref()
                                .ok_or_else(|| "в маркере отсутствует `target_dir`".to_owned())
                                .and_then(|path| match inventory::measure_path(path) {
                                    Ok(measurement) => Ok(measurement.allocated_bytes),
                                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                        Ok(0)
                                    }
                                    Err(error) => Err(format!(
                                        "не удалось измерить кэш после очистки: {error}"
                                    )),
                                });
                            match measured_after {
                                Ok(bytes_after) => {
                                    candidate.bytes_after = Some(bytes_after);
                                    candidate.bytes_freed =
                                        candidate.allocated_bytes.saturating_sub(bytes_after);
                                    candidate.reason =
                                        "кэш очищен штатной командой `cargo clean`".into();
                                }
                                Err(error) => {
                                    candidate.action = "error";
                                    candidate.error = Some(error);
                                }
                            }
                        }
                        Err(error) => {
                            candidate.action = "error";
                            candidate.error = Some(error);
                        }
                    },
                    Err(reason) => {
                        candidate.action = "deferred";
                        candidate.reason = reason;
                    }
                }
            }
        }
        cleanup_ms = cleanup_ms.saturating_add(clean_start.elapsed().as_millis());
    }
    let mut temp_apply_entries = Vec::new();
    let mut operation_errors = Vec::new();
    if options.apply {
        let clean_start = Instant::now();
        let marker_gc = cleanup_orphans_under(&options.temp_root, policy.orphan_min_age());
        match marker_gc {
            Ok(report) => {
                temp_evidence = namespace_evidence(&report.entries, &options.temp_root);
                temp_apply_entries.extend(report.entries);
            }
            Err(error) => {
                let message = format!("ошибка сборщика бесхозных каталогов по маркеру: {error}");
                operation_errors.push(message.clone());
                temp_evidence.push(OwnershipEvidence {
                    path: options.temp_root.clone(),
                    ownership: "project-owned",
                    live: None,
                    action: "error",
                    reason: message,
                });
            }
        }
        match cleanup_legacy_orphans_under(&options.temp_root, policy.orphan_min_age()) {
            Ok(report) => temp_apply_entries.extend(report.entries),
            Err(error) => {
                let message = format!("ошибка сборщика старых временных каталогов: {error}");
                operation_errors.push(message.clone());
                temp_evidence.push(OwnershipEvidence {
                    path: options.temp_root.clone(),
                    ownership: "project-owned",
                    live: None,
                    action: "error",
                    reason: message,
                });
            }
        }
        cleanup_ms = cleanup_ms.saturating_add(clean_start.elapsed().as_millis());
    }
    drop(lock);

    let project_temp = if options.apply {
        project_temp_candidates(&temp_apply_entries, true, &temp_allocated_before)
    } else if options.mode == "scan" {
        project_temp_candidates(&orphan_entries, false, &temp_allocated_before)
            .into_iter()
            .map(|mut candidate| {
                if candidate.action == "delete" {
                    candidate.action = "keep";
                    candidate.result = "scan_only";
                }
                candidate
            })
            .collect()
    } else {
        project_temp_candidates(&orphan_entries, false, &temp_allocated_before)
    };
    let mut temp_after = Vec::new();
    if options.apply {
        for temp_root in &temp_roots {
            let evidence = if temp_root == &options.temp_root {
                &temp_evidence[..]
            } else {
                &[]
            };
            let top = if options.detail {
                usize::MAX
            } else {
                options.top_entries
            };
            temp_after.push(scan_tmp(temp_root, top, evidence).map_err(|error| {
                format!(
                    "не удалось повторно просканировать {}: {error}",
                    temp_root.display()
                )
            })?);
        }
    }
    let tmp = if options.apply {
        temp_after
    } else {
        temp_before
    };
    if options.apply && target_result == "removed" {
        target_after = Some(measure_workspace_bytes(&workspace));
        target_freed = target_report
            .allocated_bytes
            .saturating_sub(target_after.unwrap_or(0));
    }
    let generated_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let external_cache_bytes_after = external_caches
        .candidates
        .iter()
        .filter(|candidate| candidate.ownership == "project-owned")
        .map(|candidate| candidate.bytes_after.unwrap_or(candidate.allocated_bytes))
        .fold(0_u64, u64::saturating_add);
    let external_cache_bytes_freed =
        external_cache_bytes_before.saturating_sub(external_cache_bytes_after);
    let fatal_errors = usize::from(target_action == "error")
        + external_caches
            .candidates
            .iter()
            .filter(|candidate| candidate.action == "error")
            .count()
        + project_temp
            .iter()
            .filter(|candidate| candidate.action == "error")
            .count()
        + operation_errors.len();
    let errors = fatal_errors + tmp.iter().map(|inventory| inventory.errors).sum::<usize>();
    Ok(Report {
        schema_version: REPORT_SCHEMA,
        mode: options.mode,
        generated_unix_seconds,
        workspace_root: root.to_path_buf(),
        scan_elapsed_ms,
        cleanup_elapsed_ms: cleanup_ms,
        elapsed_ms: started.elapsed().as_millis(),
        external_cache_bytes_before,
        external_cache_bytes_after,
        external_cache_bytes_freed,
        external_cache_unknown_entries,
        cargo_target: Candidate {
            category: "cargo-target",
            path: target_report.target_dir,
            ownership: if target_report.ownership_safe {
                "project-owned".to_owned()
            } else {
                "unknown".to_owned()
            },
            bytes_before: target_report.allocated_bytes,
            bytes_after: target_after,
            bytes_freed: target_freed,
            reason: target_report.reason,
            action: target_action,
            result: target_result,
            live: match target_report.safety {
                process::ProcessCheck::Busy { .. } => Some(true),
                process::ProcessCheck::Clear => Some(false),
                process::ProcessCheck::Unknown { .. } => None,
            },
            error: target_error,
        },
        external_build_caches: external_caches.candidates,
        project_temp,
        tmp,
        system_tmpfiles: systemd_tmpfiles_status(),
        errors,
        fatal_errors,
        operation_errors,
    })
}

enum TargetCleanOutcome {
    Removed,
    NotNeeded(ThresholdDecision),
    Deferred(String),
    Unsafe(String),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum TargetRecheckFailure {
    Busy(String),
    Unsafe(String),
}

fn clean_target_if_needed(
    workspace: &CargoWorkspace,
    initial: &CargoTargetMeasurement,
    warning_bytes: u64,
    hard_limit_bytes: u64,
) -> TargetCleanOutcome {
    let decision = match recheck_target(workspace, initial, warning_bytes, hard_limit_bytes) {
        Ok(decision) => decision,
        Err(TargetRecheckFailure::Busy(reason)) => return TargetCleanOutcome::Deferred(reason),
        Err(TargetRecheckFailure::Unsafe(reason)) => return TargetCleanOutcome::Unsafe(reason),
    };
    if decision != ThresholdDecision::Clean {
        return TargetCleanOutcome::NotNeeded(decision);
    }
    match target::cargo_clean(workspace) {
        Ok(_) => TargetCleanOutcome::Removed,
        Err(error) => TargetCleanOutcome::Failed(error),
    }
}

fn recheck_target(
    workspace: &CargoWorkspace,
    initial: &CargoTargetMeasurement,
    warning_bytes: u64,
    hard_limit_bytes: u64,
) -> Result<ThresholdDecision, TargetRecheckFailure> {
    if !initial.safe_to_clean {
        return Err(TargetRecheckFailure::Unsafe(initial.reason.clone()));
    }
    for directory in &workspace.measured_dirs {
        let current = inventory::measure_path(directory).map_err(|error| {
            TargetRecheckFailure::Unsafe(format!(
                "не удалось перепроверить {}: {error}",
                directory.display()
            ))
        })?;
        if let Some(before) = initial.dirs.iter().find(|item| item.path == *directory)
            && (before.device != current.device || before.inode != current.inode)
        {
            return Err(TargetRecheckFailure::Unsafe(format!(
                "{} заменён после scan",
                directory.display()
            )));
        }
        if current.mount_boundaries != 0 || !current.errors.is_empty() {
            return Err(TargetRecheckFailure::Unsafe(format!(
                "{} изменился во время scan",
                directory.display()
            )));
        }
    }
    for directory in &workspace.measured_dirs {
        match process::target_process_check(directory, &workspace.root) {
            process::ProcessCheck::Clear => (),
            process::ProcessCheck::Busy {
                pid,
                process,
                reference,
            } => {
                return Err(TargetRecheckFailure::Busy(format!(
                    "активен {process} PID {pid}: {reference}"
                )));
            }
            process::ProcessCheck::Unknown { reason } => {
                return Err(TargetRecheckFailure::Unsafe(format!(
                    "нельзя доказать отсутствие активного процесса: {reason}"
                )));
            }
        }
    }
    target::validate_cargo_target(&workspace.target_dir).map_err(|error| {
        TargetRecheckFailure::Unsafe(format!("маркер Cargo изменился до очистки: {error}"))
    })?;

    // Размер проверяется последним, после повторной проверки процессов и маркера:
    // уменьшившийся ниже жёсткого предела `target` больше не требует очистки.
    let mut allocated_bytes = 0_u64;
    for directory in &workspace.measured_dirs {
        let current = inventory::measure_path(directory).map_err(|error| {
            TargetRecheckFailure::Unsafe(format!(
                "не удалось повторно измерить {}: {error}",
                directory.display()
            ))
        })?;
        if let Some(before) = initial.dirs.iter().find(|item| item.path == *directory)
            && (before.device != current.device || before.inode != current.inode)
        {
            return Err(TargetRecheckFailure::Unsafe(format!(
                "{} заменён перед очисткой",
                directory.display()
            )));
        }
        if current.mount_boundaries != 0 || !current.errors.is_empty() {
            return Err(TargetRecheckFailure::Unsafe(format!(
                "{} изменился перед очисткой",
                directory.display()
            )));
        }
        allocated_bytes = allocated_bytes.saturating_add(current.allocated_bytes);
    }
    Ok(target::threshold_decision(
        allocated_bytes,
        warning_bytes,
        hard_limit_bytes,
    ))
}

fn recheck_external_cache(
    repository_root: &Path,
    candidate: &cache::ExternalCacheCandidate,
) -> Result<(), String> {
    if candidate.ownership != "project-owned" || candidate.live != Some(false) {
        return Err("кэш не подтверждён как безопасный для очистки".into());
    }
    let target_dir = candidate
        .target_dir
        .as_deref()
        .ok_or_else(|| "в маркере отсутствует `target_dir`".to_owned())?;
    target::validate_cargo_target(target_dir)
        .map_err(|error| format!("маркер Cargo изменился до очистки: {error}"))?;
    let fresh = cache::inspect_external_caches(
        repository_root,
        candidate
            .path
            .parent()
            .ok_or("не найден корень внешних кэшей")?,
        Duration::ZERO,
    )
    .map_err(|error| format!("не удалось повторно проверить маркер владения: {error}"))?;
    let current = fresh
        .candidates
        .iter()
        .find(|item| item.id == candidate.id)
        .ok_or_else(|| "кэш исчез после проверки".to_owned())?;
    if current.ownership != "project-owned"
        || current.target_dir != candidate.target_dir
        || current.last_used_unix_seconds != candidate.last_used_unix_seconds
        || current.live != Some(false)
    {
        return Err("владение кэшем или состояние процесса изменились после проверки".into());
    }
    let before = candidate
        .measurement
        .as_ref()
        .ok_or_else(|| "измерение кэша отсутствует".to_owned())?;
    let after = inventory::measure_path(target_dir)
        .map_err(|error| format!("не удалось повторно измерить кэш: {error}"))?;
    if before.device != after.device || before.inode != after.inode {
        return Err("каталог `target` кэша заменён после проверки".into());
    }
    if after.mount_boundaries != 0 || !after.errors.is_empty() {
        return Err("каталог `target` кэша содержит границы точек монтирования или ошибки".into());
    }
    match process::target_process_check(target_dir, repository_root) {
        process::ProcessCheck::Clear => Ok(()),
        process::ProcessCheck::Busy {
            pid,
            process,
            reference,
        } => Err(format!("активен {process} PID {pid}: {reference}")),
        process::ProcessCheck::Unknown { reason } => Err(format!(
            "нельзя доказать отсутствие активного процесса: {reason}"
        )),
    }
}

fn measure_workspace_bytes(workspace: &CargoWorkspace) -> u64 {
    workspace
        .measured_dirs
        .iter()
        .filter_map(|path| inventory::measure_path(path).ok())
        .map(|measurement| measurement.allocated_bytes)
        .fold(0_u64, u64::saturating_add)
}

fn namespace_evidence(entries: &[GcEntry], root: &Path) -> Vec<OwnershipEvidence> {
    let mut groups: BTreeMap<PathBuf, Vec<&GcEntry>> = BTreeMap::new();
    for entry in entries {
        let Some(parent) = entry.path.parent() else {
            continue;
        };
        groups.entry(parent.to_path_buf()).or_default().push(entry);
    }
    groups
        .into_iter()
        .filter(|(path, _)| path.starts_with(root))
        .map(|(path, entries)| {
            let all_owned = entries
                .iter()
                .all(|entry| entry.ownership == "project-owned");
            let live = if entries.iter().any(|entry| entry.live == Some(true)) {
                Some(true)
            } else if entries.iter().any(|entry| entry.live.is_none()) {
                None
            } else {
                Some(false)
            };
            OwnershipEvidence {
                path,
                ownership: if all_owned {
                    "project-owned"
                } else {
                    "unknown"
                },
                live,
                action: "keep",
                reason: if all_owned {
                    "пространство имён содержит запуски `TempWorkspace` с проверенными маркерами владения".into()
                } else {
                    "маркеры владения подтверждают не все дочерние записи".into()
                },
            }
        })
        .collect()
}

fn project_temp_candidates(
    entries: &[GcEntry],
    apply: bool,
    bytes_before: &BTreeMap<PathBuf, u64>,
) -> Vec<Candidate> {
    entries
        .iter()
        .map(|entry| {
            let (action, result) = match entry.outcome.as_str() {
                "delete" => ("delete", if apply { "removed" } else { "planned" }),
                "removed" => ("delete", "removed"),
                "error" => ("error", "failed"),
                "deferred" if entry.live == Some(true) => ("deferred", "busy"),
                "deferred" => ("deferred", "unverified_or_fresh"),
                _ => ("keep", "skipped"),
            };
            let before = bytes_before
                .get(&entry.path)
                .copied()
                .unwrap_or(entry.bytes);
            let after = if result == "removed" {
                Some(0)
            } else {
                inventory::measure_path(&entry.path)
                    .ok()
                    .map(|measurement| measurement.allocated_bytes)
            };
            Candidate {
                category: "temp-workspace",
                path: entry.path.clone(),
                ownership: entry.ownership.clone(),
                bytes_before: before,
                bytes_after: after,
                bytes_freed: before.saturating_sub(after.unwrap_or(before)),
                reason: entry.reason.clone(),
                action,
                result,
                live: entry.live,
                error: (action == "error").then(|| entry.reason.clone()),
            }
        })
        .collect()
}

pub fn systemd_tmpfiles_status() -> SystemTmpfilesStatus {
    let command = std::process::Command::new("systemctl")
        .args(["is-active", "systemd-tmpfiles-clean.timer"])
        .output();
    match command {
        Ok(output) => {
            let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            SystemTmpfilesStatus {
                available: true,
                timer_state: if output.status.success() {
                    state.clone()
                } else {
                    state
                },
                message: if output.status.success() {
                    "обнаружен системный таймер очистки временных файлов".into()
                } else {
                    format!("таймер не активен или отсутствует: {stderr}")
                },
            }
        }
        Err(error) => SystemTmpfilesStatus {
            available: false,
            timer_state: "unknown".into(),
            message: format!("systemctl недоступен: {error}"),
        },
    }
}

pub fn render_text(report: &Report) -> String {
    let mut text = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(text, "Режим: {}", report.mode);
    let _ = writeln!(
        text,
        "Рабочая область: {}",
        display_report_path(&report.workspace_root)
    );
    let _ = writeln!(
        text,
        "Время: проверка {} мс, очистка {} мс, всего {} мс",
        report.scan_elapsed_ms, report.cleanup_elapsed_ms, report.elapsed_ms
    );
    let _ = writeln!(
        text,
        "Каталог Cargo `target`: {} — {} ({}, владение {}, освобождено {} байт)",
        display_report_path(&report.cargo_target.path),
        human_bytes(report.cargo_target.bytes_before),
        report.cargo_target.action,
        report.cargo_target.ownership,
        report.cargo_target.bytes_freed
    );
    let _ = writeln!(text, "Причина: {}", report.cargo_target.reason);
    if let Some(error) = &report.cargo_target.error {
        let _ = writeln!(text, "Ошибка: {error}");
    }
    let _ = writeln!(
        text,
        "Внешние кэши сборки, принадлежащие проекту: {} байт до, {} после, освобождено {} байт; неизвестных объектов {}",
        report.external_cache_bytes_before,
        report.external_cache_bytes_after,
        report.external_cache_bytes_freed,
        report.external_cache_unknown_entries
    );
    for candidate in &report.external_build_caches {
        let _ = writeln!(
            text,
            "  {} — {} байт до, {} после, освобождено {}; {}, {}: {}",
            display_report_path(&candidate.path),
            candidate.allocated_bytes,
            candidate
                .bytes_after
                .map_or_else(|| "?".into(), |value| value.to_string()),
            candidate.bytes_freed,
            candidate.action,
            candidate.ownership,
            candidate.reason
        );
        if let Some(error) = &candidate.error {
            let _ = writeln!(text, "    Ошибка: {error}");
        }
    }
    let _ = writeln!(
        text,
        "Кандидаты `TempWorkspace`: {}",
        report.project_temp.len()
    );
    for candidate in &report.project_temp {
        let _ = writeln!(
            text,
            "  {} — {} байт, владение {}, {} / {}: {}",
            display_report_path(&candidate.path),
            candidate.bytes_before,
            candidate.ownership,
            candidate.action,
            candidate.result,
            candidate.reason
        );
    }
    for inventory in &report.tmp {
        let _ = writeln!(
            text,
            "/tmp {}: {}, {} записей верхнего уровня, {} границ точек монтирования, {} ошибок проверки; обход {}",
            display_report_path(&inventory.root),
            human_bytes(inventory.allocated_bytes),
            inventory.top_level_entries,
            inventory.mount_boundaries,
            inventory.errors,
            if inventory.complete {
                "полный"
            } else {
                "неполный"
            }
        );
        for entry in &inventory.largest {
            let age = entry
                .age_seconds
                .map_or_else(|| "?".into(), |age| format!("{}с", age));
            let _ = writeln!(
                text,
                "  {} — {}, {}, {}, возраст {}, {}",
                display_report_path(&entry.path),
                human_bytes(entry.allocated_bytes),
                entry.kind,
                entry.ownership,
                age,
                entry.action
            );
        }
    }
    let _ = writeln!(
        text,
        "Состояние systemd-tmpfiles: {} ({})",
        report.system_tmpfiles.timer_state, report.system_tmpfiles.message
    );
    let _ = writeln!(
        text,
        "Ошибки: {} (препятствуют успешному завершению: {})",
        report.errors, report.fatal_errors
    );
    for error in &report.operation_errors {
        let _ = writeln!(text, "  Ошибка операции обслуживания: {error}");
    }
    text
}

pub fn render_json(report: &Report) -> Result<String, String> {
    serde_json::to_string_pretty(report)
        .map_err(|error| format!("не удалось сериализовать JSON: {error}"))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn synthetic_target(root: &Path) -> (target::CargoWorkspace, CargoTargetMeasurement) {
        let target_dir = root.join("target");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(
            target_dir.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55\nGenerated by Cargo\n",
        )
        .unwrap();
        fs::write(target_dir.join("artifact"), [0x31_u8; 4096]).unwrap();
        let workspace = target::CargoWorkspace {
            root: root.to_path_buf(),
            manifest: root.join("Cargo.toml"),
            target_dir: target_dir.clone(),
            build_dir: None,
            measured_dirs: vec![target_dir.clone()],
        };
        let measurement = inventory::measure_path(&target_dir).unwrap();
        let initial = CargoTargetMeasurement {
            workspace_root: root.to_path_buf(),
            target_dir,
            allocated_bytes: measurement.allocated_bytes,
            apparent_bytes: measurement.apparent_bytes,
            dirs: vec![measurement],
            decision: ThresholdDecision::Clean,
            safety: process::ProcessCheck::Clear,
            safe_to_clean: true,
            ownership_safe: true,
            blocker: None,
            reason: "синтетическое тестовое измерение".into(),
        };
        (workspace, initial)
    }

    #[test]
    fn target_recheck_defers_if_directory_disappeared_after_scan() {
        let owner = asset_store::temp_workspace::TempWorkspace::create(
            "repository-maintenance-disappeared-target-test",
        )
        .unwrap();
        let root = owner.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let (workspace, initial) = synthetic_target(&root);
        fs::remove_dir_all(&initial.target_dir).unwrap();

        let error = recheck_target(&workspace, &initial, 1, 1).unwrap_err();
        assert!(matches!(
            error,
            TargetRecheckFailure::Unsafe(message)
                if message.contains("не удалось перепроверить")
        ));
    }

    #[test]
    fn target_recheck_defers_if_directory_inode_changed_after_scan() {
        let owner = asset_store::temp_workspace::TempWorkspace::create(
            "repository-maintenance-replaced-target-test",
        )
        .unwrap();
        let root = owner.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let (workspace, initial) = synthetic_target(&root);
        let replaced = root.join("target-before-replacement");
        fs::rename(&initial.target_dir, &replaced).unwrap();
        fs::create_dir(&initial.target_dir).unwrap();
        fs::write(
            initial.target_dir.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55\nGenerated by Cargo\n",
        )
        .unwrap();
        fs::write(initial.target_dir.join("artifact"), [0x32_u8; 4096]).unwrap();

        let error = recheck_target(&workspace, &initial, 1, 1).unwrap_err();
        assert!(matches!(
            error,
            TargetRecheckFailure::Unsafe(message)
                if message.contains("заменён после scan")
        ));
        assert!(replaced.join("artifact").exists());
    }

    #[test]
    fn target_recheck_skips_clean_when_size_falls_below_hard_limit() {
        let owner = asset_store::temp_workspace::TempWorkspace::create(
            "repository-maintenance-smaller-target-test",
        )
        .unwrap();
        let root = owner.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let (workspace, initial) = synthetic_target(&root);
        let artifact = initial.target_dir.join("artifact");
        fs::write(&artifact, []).unwrap();
        let initial_metadata = fs::metadata(&initial.target_dir).unwrap();
        let outcome = clean_target_if_needed(&workspace, &initial, 0, initial.allocated_bytes);
        let final_metadata = fs::metadata(&initial.target_dir).unwrap();

        assert!(
            initial.allocated_bytes
                > inventory::measure_path(&initial.target_dir)
                    .unwrap()
                    .allocated_bytes
        );
        assert!(matches!(
            outcome,
            TargetCleanOutcome::NotNeeded(ThresholdDecision::Warn)
        ));
        assert_eq!(initial_metadata.ino(), final_metadata.ino());
        assert!(
            artifact.exists(),
            "повторная проверка не удаляет содержимое"
        );
    }

    #[test]
    fn project_temp_report_preserves_legacy_ownership() {
        let owner = asset_store::temp_workspace::TempWorkspace::create(
            "repository-maintenance-temp-ownership-test",
        )
        .unwrap();
        let path = owner.path().join("legacy-run");
        let entry = GcEntry {
            path: path.clone(),
            outcome: "deferred".into(),
            reason: "старый маркер".into(),
            bytes: 0,
            ownership: "legacy-project-owned".into(),
            live: Some(false),
        };

        let candidates = project_temp_candidates(&[entry], false, &BTreeMap::new());

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].ownership, "legacy-project-owned");
    }

    #[test]
    fn reports_hide_temp_namespace_uids_and_keep_decisions() {
        let candidate = Candidate {
            category: "cargo-target",
            path: PathBuf::from("/tmp/anki-decks-424242/target"),
            ownership: "project-owned".into(),
            bytes_before: 12,
            bytes_after: None,
            bytes_freed: 0,
            reason: "ниже порога".into(),
            action: "keep",
            result: "not_needed",
            live: Some(false),
            error: None,
        };
        let report = Report {
            schema_version: REPORT_SCHEMA,
            mode: "dry-run",
            generated_unix_seconds: 1,
            workspace_root: PathBuf::from("/work/anki-decks-424242/checkout"),
            scan_elapsed_ms: 2,
            cleanup_elapsed_ms: 0,
            elapsed_ms: 2,
            external_cache_bytes_before: 0,
            external_cache_bytes_after: 0,
            external_cache_bytes_freed: 0,
            external_cache_unknown_entries: 0,
            cargo_target: candidate,
            external_build_caches: vec![
                cache::ExternalCacheCandidate {
                    id: "cache".into(),
                    path: PathBuf::from("/tmp/anki-decks-424242/cache"),
                    target_dir: Some(PathBuf::from("/tmp/anki-decks-424242/cache/target")),
                    allocated_bytes: 12,
                    bytes_after: None,
                    bytes_freed: 0,
                    last_used_unix_seconds: None,
                    age_seconds: None,
                    ownership: "project-owned",
                    live: None,
                    action: "keep",
                    reason: "проверенный кэш".into(),
                    error: None,
                    measurement: None,
                },
                cache::ExternalCacheCandidate {
                    id: "unknown".into(),
                    path: PathBuf::from("/tmp/unknown-cache"),
                    target_dir: None,
                    allocated_bytes: 0,
                    bytes_after: None,
                    bytes_freed: 0,
                    last_used_unix_seconds: None,
                    age_seconds: None,
                    ownership: "unknown",
                    live: None,
                    action: "keep",
                    reason: "неизвестный кэш".into(),
                    error: None,
                    measurement: None,
                },
            ],
            project_temp: vec![Candidate {
                category: "temp-workspace",
                path: PathBuf::from("/tmp/anki-decks-424242/legacy-run"),
                ownership: "project-owned".into(),
                bytes_before: 12,
                bytes_after: Some(12),
                bytes_freed: 0,
                reason: "проверенный временный каталог".into(),
                action: "keep",
                result: "skipped",
                live: None,
                error: None,
            }],
            tmp: vec![TmpInventory {
                root: PathBuf::from("/tmp/anki-decks-424242"),
                allocated_bytes: 12,
                top_level_entries: 1,
                mount_boundaries: 0,
                errors: 0,
                complete: true,
                largest: vec![inventory::TmpEntry {
                    path: PathBuf::from("/tmp/anki-decks-424242/foreign-entry"),
                    kind: "file",
                    allocated_bytes: 12,
                    apparent_bytes: 12,
                    modified_unix_seconds: Some(1),
                    age_seconds: Some(2),
                    owner_uid: Some(424_242),
                    ownership: "foreign",
                    live: None,
                    action: "foreign",
                    reason: "объект принадлежит другому UID".into(),
                    error: None,
                }],
            }],
            system_tmpfiles: SystemTmpfilesStatus {
                available: false,
                timer_state: "unknown".into(),
                message: "нет".into(),
            },
            errors: 0,
            fatal_errors: 0,
            operation_errors: Vec::new(),
        };
        let json = render_json(&report).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"action\": \"keep\""));
        assert!(json.contains("\"ownership\": \"foreign\""));
        assert!(!json.contains("owner_uid"));
        assert!(!json.contains("424242"));
        assert!(json.contains("/tmp/anki-decks-<uid>/cache/target"));
        assert!(json.contains("/tmp/anki-decks-<uid>/foreign-entry"));
        assert!(json.contains("\"target_dir\": null"));
        assert_eq!(json, render_json(&report).unwrap());

        let text = render_text(&report);
        assert!(text.contains("/tmp/anki-decks-<uid>/foreign-entry"));
        assert!(text.contains("/tmp/anki-decks-<uid>/cache"));
        assert!(text.contains("file, foreign, возраст"));
        assert!(!text.contains("424242"));
        assert!(text.contains("/work/anki-decks-<uid>/checkout"));
    }

    #[test]
    fn report_path_redaction_leaves_other_components_unchanged() {
        assert_eq!(
            display_report_path(Path::new("/tmp/anki-decks-123abc/entry-42")),
            "/tmp/anki-decks-123abc/entry-42"
        );
        assert_eq!(
            display_report_path(Path::new("/tmp/not-anki-decks-123/entry")),
            "/tmp/not-anki-decks-123/entry"
        );
    }
}
