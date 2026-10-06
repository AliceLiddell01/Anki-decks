pub mod cache;
pub mod guard;
pub mod inventory;
pub mod policy;
pub mod process;
pub mod target;
pub mod timer;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use asset_store::temp_workspace::{
    GcEntry, cleanup_legacy_orphans_under, cleanup_orphans_under, plan_legacy_orphans_under,
    plan_orphans_under,
};
use inventory::{OwnershipEvidence, TmpInventory, scan_tmp};
use policy::Policy;
use serde::Serialize;
use target::{CargoTargetMeasurement, CargoWorkspace, ThresholdDecision};

const REPORT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub category: &'static str,
    pub path: PathBuf,
    pub ownership: &'static str,
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
                    "очистка отложена: не удалось получить maintenance lock: {error}"
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
        .map_err(|error| format!("не удалось инвентаризировать external build caches: {error}"))?,
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
        .map_err(|error| format!("не удалось построить план TempWorkspace GC: {error}"))?;
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
    if target_report.decision == ThresholdDecision::Clean && target_report.safe_to_clean {
        let clean_start = Instant::now();
        if options.apply {
            let safety = recheck_target(&workspace, &target_report);
            match safety {
                Ok(()) => match target::cargo_clean(&workspace) {
                    Ok((_stdout, _stderr)) => {
                        target_result = "removed";
                        target_after = Some(measure_workspace_bytes(&workspace));
                        target_freed = target_report
                            .allocated_bytes
                            .saturating_sub(target_after.unwrap_or(0));
                    }
                    Err(error) => {
                        target_action = "error";
                        target_result = "failed";
                        target_error = Some(error);
                    }
                },
                Err(reason) => {
                    target_action = "deferred";
                    target_result = "busy";
                    target_error = Some(reason);
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
                        .ok_or_else(|| "в marker отсутствует target_dir".to_owned())
                        .and_then(|target_dir| cache::clean_cache(root, target_dir))
                    {
                        Ok(_) => {
                            let measured_after = candidate
                                .target_dir
                                .as_deref()
                                .ok_or_else(|| "в marker отсутствует target_dir".to_owned())
                                .and_then(|path| match inventory::measure_path(path) {
                                    Ok(measurement) => Ok(measurement.allocated_bytes),
                                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                        Ok(0)
                                    }
                                    Err(error) => Err(format!(
                                        "не удалось измерить cache после очистки: {error}"
                                    )),
                                });
                            match measured_after {
                                Ok(bytes_after) => {
                                    candidate.bytes_after = Some(bytes_after);
                                    candidate.bytes_freed =
                                        candidate.allocated_bytes.saturating_sub(bytes_after);
                                    candidate.reason =
                                        "cache очищен штатной командой cargo clean".into();
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
    if options.apply {
        let clean_start = Instant::now();
        let marker_gc = cleanup_orphans_under(&options.temp_root, policy.orphan_min_age());
        match marker_gc {
            Ok(report) => {
                temp_evidence = namespace_evidence(&report.entries, &options.temp_root);
                temp_apply_entries.extend(report.entries);
            }
            Err(error) => temp_evidence.push(OwnershipEvidence {
                path: options.temp_root.clone(),
                ownership: "project-owned",
                live: None,
                action: "error",
                reason: format!("ошибка marker orphan GC: {error}"),
            }),
        }
        match cleanup_legacy_orphans_under(&options.temp_root, policy.orphan_min_age()) {
            Ok(report) => temp_apply_entries.extend(report.entries),
            Err(error) => temp_evidence.push(OwnershipEvidence {
                path: options.temp_root.clone(),
                ownership: "project-owned",
                live: None,
                action: "error",
                reason: format!("ошибка legacy orphan GC: {error}"),
            }),
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
    let errors = usize::from(target_action == "error")
        + external_caches
            .candidates
            .iter()
            .filter(|candidate| candidate.action == "error")
            .count()
        + project_temp
            .iter()
            .filter(|candidate| candidate.action == "error")
            .count()
        + tmp.iter().map(|inventory| inventory.errors).sum::<usize>();
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
                "project-owned"
            } else {
                "unknown"
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
    })
}

fn recheck_target(
    workspace: &CargoWorkspace,
    initial: &CargoTargetMeasurement,
) -> Result<(), String> {
    if !initial.safe_to_clean {
        return Err(initial.reason.clone());
    }
    for directory in &workspace.measured_dirs {
        let current = inventory::measure_path(directory).map_err(|error| {
            format!("не удалось перепроверить {}: {error}", directory.display())
        })?;
        if let Some(before) = initial.dirs.iter().find(|item| item.path == *directory)
            && (before.device != current.device || before.inode != current.inode)
        {
            return Err(format!("{} заменён после scan", directory.display()));
        }
        if current.mount_boundaries != 0 || !current.errors.is_empty() {
            return Err(format!("{} изменился во время scan", directory.display()));
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
                return Err(format!("активен {process} PID {pid}: {reference}"));
            }
            process::ProcessCheck::Unknown { reason } => {
                return Err(format!(
                    "нельзя доказать отсутствие активного процесса: {reason}"
                ));
            }
        }
    }
    target::validate_cargo_target(&workspace.target_dir)
        .map_err(|error| format!("Cargo marker изменился до очистки: {error}"))
}

fn recheck_external_cache(
    repository_root: &Path,
    candidate: &cache::ExternalCacheCandidate,
) -> Result<(), String> {
    if candidate.ownership != "project-owned" || candidate.live != Some(false) {
        return Err("cache не подтверждён как безопасный для очистки".into());
    }
    let target_dir = candidate
        .target_dir
        .as_deref()
        .ok_or_else(|| "в marker отсутствует target_dir".to_owned())?;
    target::validate_cargo_target(target_dir)
        .map_err(|error| format!("Cargo marker изменился до очистки: {error}"))?;
    let fresh = cache::inspect_external_caches(
        repository_root,
        candidate.path.parent().ok_or("cache root отсутствует")?,
        Duration::ZERO,
    )
    .map_err(|error| format!("не удалось перепроверить ownership marker: {error}"))?;
    let current = fresh
        .candidates
        .iter()
        .find(|item| item.id == candidate.id)
        .ok_or_else(|| "cache исчез после scan".to_owned())?;
    if current.ownership != "project-owned"
        || current.target_dir != candidate.target_dir
        || current.last_used_unix_seconds != candidate.last_used_unix_seconds
        || current.live != Some(false)
    {
        return Err("ownership или process state cache изменились после scan".into());
    }
    let before = candidate
        .measurement
        .as_ref()
        .ok_or_else(|| "измерение cache отсутствует".to_owned())?;
    let after = inventory::measure_path(target_dir)
        .map_err(|error| format!("не удалось повторно измерить cache: {error}"))?;
    if before.device != after.device || before.inode != after.inode {
        return Err("cache target заменён после scan".into());
    }
    if after.mount_boundaries != 0 || !after.errors.is_empty() {
        return Err("cache target содержит mount boundary или ошибки".into());
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
                    "namespace содержит marker-verified TempWorkspace runs".into()
                } else {
                    "не все дочерние записи подтверждены ownership marker".into()
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
                ownership: "project-owned",
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
                    "системный таймер очистки temporary files обнаружен".into()
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
    let _ = writeln!(text, "Workspace: {}", report.workspace_root.display());
    let _ = writeln!(
        text,
        "Время: scan {} мс, cleanup {} мс, всего {} мс",
        report.scan_elapsed_ms, report.cleanup_elapsed_ms, report.elapsed_ms
    );
    let _ = writeln!(
        text,
        "Cargo target: {} — {} ({}, ownership {}, освобождено {} байт)",
        report.cargo_target.path.display(),
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
        "Внешние project-owned build caches: {} байт до, {} после, освобождено {} байт; неизвестных объектов {}",
        report.external_cache_bytes_before,
        report.external_cache_bytes_after,
        report.external_cache_bytes_freed,
        report.external_cache_unknown_entries
    );
    for candidate in &report.external_build_caches {
        let _ = writeln!(
            text,
            "  {} — {} байт до, {} после, освобождено {}; {}, {}: {}",
            candidate.path.display(),
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
        "TempWorkspace candidates: {}",
        report.project_temp.len()
    );
    for candidate in &report.project_temp {
        let _ = writeln!(
            text,
            "  {} — {} байт, ownership {}, {} / {}: {}",
            candidate.path.display(),
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
            "/tmp {}: {}, {} top-level entries, {} mount boundaries, {} scan errors, обход {}",
            inventory.root.display(),
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
            let owner = entry
                .owner_uid
                .map_or_else(|| "mixed/unknown".into(), |uid| uid.to_string());
            let age = entry
                .age_seconds
                .map_or_else(|| "?".into(), |age| format!("{}с", age));
            let _ = writeln!(
                text,
                "  {} — {}, {}, UID {}, {}, возраст {}, {}",
                entry.path.display(),
                human_bytes(entry.allocated_bytes),
                entry.kind,
                owner,
                entry.ownership,
                age,
                entry.action
            );
        }
    }
    let _ = writeln!(
        text,
        "systemd-tmpfiles: {} ({})",
        report.system_tmpfiles.timer_state, report.system_tmpfiles.message
    );
    let _ = writeln!(text, "Ошибки: {}", report.errors);
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
            reason: "synthetic test measurement".into(),
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

        let error = recheck_target(&workspace, &initial).unwrap_err();
        assert!(error.contains("не удалось перепроверить"));
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

        let error = recheck_target(&workspace, &initial).unwrap_err();
        assert!(error.contains("заменён после scan"));
        assert!(replaced.join("artifact").exists());
    }

    #[test]
    fn json_report_has_stable_schema_and_explicit_decisions() {
        let candidate = Candidate {
            category: "cargo-target",
            path: PathBuf::from("/tmp/test/target"),
            ownership: "project-owned",
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
            workspace_root: PathBuf::from("/work"),
            scan_elapsed_ms: 2,
            cleanup_elapsed_ms: 0,
            elapsed_ms: 2,
            external_cache_bytes_before: 0,
            external_cache_bytes_after: 0,
            external_cache_bytes_freed: 0,
            external_cache_unknown_entries: 0,
            cargo_target: candidate,
            external_build_caches: Vec::new(),
            project_temp: Vec::new(),
            tmp: Vec::new(),
            system_tmpfiles: SystemTmpfilesStatus {
                available: false,
                timer_state: "unknown".into(),
                message: "нет".into(),
            },
            errors: 0,
        };
        let json = render_json(&report).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"action\": \"keep\""));
        assert_eq!(json, render_json(&report).unwrap());
    }
}
