//! Общая диагностика обслуживания для человека и автоматизации.
//! Чтение статуса никогда не устанавливает таймер и не меняет данные пользователя.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::guard::MaintenanceLock;
use crate::policy::{Policy, config_home};
use crate::{Report, RunOptions, run, timer};

const WARNING_INTERVAL: u64 = 24 * 60 * 60;
const MAX_STATE_BYTES: u64 = 32 * 1024;
const MAX_REPORTED_ITEMS: usize = 24;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceFile {
    schema_version: u32,
    choice: String,
    last_warning_at: Option<u64>,
}

/// Краткая запись о проверенном объекте; оригинальная политика GC не дублируется.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcItem {
    pub category: String,
    #[serde(serialize_with = "crate::serialize_report_path")]
    pub path: PathBuf,
    pub action: String,
    pub result: String,
    pub bytes_freed: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastAttempt {
    pub schema_version: u32,
    pub workspace_root: PathBuf,
    pub timestamp: u64,
    pub outcome: String,
    pub bytes_freed: Option<u64>,
    pub errors: usize,
    pub inventory_complete: bool,
    pub message: String,
    #[serde(default)]
    pub items: Vec<GcItem>,
    #[serde(default)]
    pub omitted_items: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: &'static str,
    pub message: String,
    pub action: Option<&'static str>,
    pub context: serde_json::Value,
}

/// Состояние полного обзора системных временных каталогов (включая чужие).
#[derive(Clone, Debug, Serialize)]
pub struct SystemTmpHealth {
    pub complete: bool,
    pub errors: usize,
    pub mount_boundaries: usize,
}

impl SystemTmpHealth {
    fn from_inventories(inventories: &[crate::inventory::TmpInventory]) -> Self {
        Self {
            complete: inventories.iter().all(|item| item.complete),
            errors: inventories.iter().map(|item| item.errors).sum(),
            mount_boundaries: inventories.iter().map(|item| item.mount_boundaries).sum(),
        }
    }
}

fn system_tmp_diagnostic(health: &SystemTmpHealth) -> Option<Diagnostic> {
    if health.complete {
        return None;
    }
    let mut entry = diag(
        "system_tmp_inventory_incomplete",
        "info",
        "Общий обзор /tmp неполон; показатели подтверждённых артефактов Anki-decks учитываются отдельно. Чужие каталоги и mount не обходятся.",
        Some("anki-repository-maintenance scan --detail --json"),
    );
    entry.context = serde_json::json!({
        "errors": health.errors,
        "mount_boundaries": health.mount_boundaries,
    });
    Some(entry)
}

#[derive(Clone, Debug, Serialize)]
pub struct Measurement {
    pub project_bytes: Option<u64>,
    pub complete: bool,
    pub cargo_target_bytes: Option<u64>,
    pub target_warning_bytes: u64,
    pub target_hard_limit_bytes: u64,
    pub external_cache_bytes: Option<u64>,
    pub external_cache_hard_limit_bytes: u64,
    pub deferred: usize,
    /// Всего ошибок в полном отчёте, включая чужой /tmp.
    pub errors: usize,
    /// Статус общесистемного /tmp, который не равен полноте проекта.
    pub system_tmp: Option<SystemTmpHealth>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CareStatus {
    pub schema_version: u32,
    pub workspace_root: PathBuf,
    pub preference: String,
    pub automatic: String,
    pub installed_workspace: Option<PathBuf>,
    pub timer_active: Option<bool>,
    pub measurement: Measurement,
    pub items: Vec<GcItem>,
    pub omitted_items: usize,
    pub last_attempt: Option<LastAttempt>,
    pub diagnostics: Vec<Diagnostic>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn pref_path() -> Result<PathBuf, String> {
    Ok(config_home()
        .ok_or("не задан HOME / XDG_CONFIG_HOME")?
        .join("anki-decks/gc-preference.json"))
}

fn state_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or("не задан HOME / XDG_STATE_HOME")?;
    Ok(base.join("anki-decks/gc-last-attempt.json"))
}

fn private_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("нет родительского каталога")?;
    if let Some(grandparent) = parent.parent() {
        fs::create_dir_all(grandparent).map_err(|e| e.to_string())?;
    }
    match fs::symlink_metadata(parent) {
        Ok(meta) => {
            if !meta.is_dir()
                || meta.file_type().is_symlink()
                || meta.uid() != fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid()
            {
                return Err(format!("недоверенный каталог {}", parent.display()));
            }
            if meta.mode() & 0o077 != 0 {
                return Err(format!(
                    "каталог {} должен иметь права 0700",
                    parent.display()
                ));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(parent).map_err(|e| e.to_string())?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    if !meta.is_file()
        || meta.file_type().is_symlink()
        || meta.uid() != uid
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > MAX_STATE_BYTES
    {
        return Err(format!("небезопасный файл состояния {}", path.display()));
    }
    fs::read(path).map(Some).map_err(|e| e.to_string())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err("превышен предел размера состояния".into());
    }
    private_parent(path)?;
    let _ = read_private(path)?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let temp = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        fs::File::open(path.parent().ok_or("нет родительского каталога")?)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn read_pref() -> Result<PreferenceFile, String> {
    let Some(bytes) = read_private(&pref_path()?)? else {
        return Ok(PreferenceFile {
            schema_version: 1,
            choice: "unset".into(),
            last_warning_at: None,
        });
    };
    let p: PreferenceFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("неверная конфигурация GC: {e}"))?;
    if p.schema_version != 1 || !matches!(p.choice.as_str(), "yes" | "no") {
        return Err("неподдерживаемое пользовательское предпочтение GC".into());
    }
    Ok(p)
}

/// Быстрая проверка решения без полной инвентаризации файловой системы.
pub fn choice() -> Result<String, String> {
    Ok(read_pref()?.choice)
}

/// Сохраняет явное решение пользователя под общей межпроцессной блокировкой.
pub fn set_choice(choice: &str) -> Result<(), String> {
    if !matches!(choice, "yes" | "no") {
        return Err("допускаются только yes или no".into());
    }
    let _lock = MaintenanceLock::acquire().map_err(|e| format!("блокировка настройки GC: {e}"))?;
    let mut p = read_pref()?;
    p.choice = choice.into();
    write_private(
        &pref_path()?,
        &serde_json::to_vec_pretty(&p).map_err(|e| e.to_string())?,
    )
}

/// Проверяет суточный интервал без расходования его на незапущенную диагностику.
pub fn warning_due() -> Result<bool, String> {
    let p = read_pref()?;
    Ok(p.choice == "no"
        && !p
            .last_warning_at
            .is_some_and(|prev| now().saturating_sub(prev) < WARNING_INTERVAL))
}

/// Текстовое предупреждение ограничено по частоте; машинная диагностика — нет.
pub fn should_warn() -> Result<bool, String> {
    let _lock = MaintenanceLock::acquire().map_err(|e| e.to_string())?;
    let mut p = read_pref()?;
    if p.choice != "no"
        || p.last_warning_at
            .is_some_and(|prev| now().saturating_sub(prev) < WARNING_INTERVAL)
    {
        return Ok(false);
    }
    p.last_warning_at = Some(now());
    write_private(
        &pref_path()?,
        &serde_json::to_vec_pretty(&p).map_err(|e| e.to_string())?,
    )?;
    Ok(true)
}

pub fn last_attempt() -> Result<Option<LastAttempt>, String> {
    let Some(bytes) = read_private(&state_path()?)? else {
        return Ok(None);
    };
    let entry: LastAttempt = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if entry.schema_version != 1 {
        return Err("неподдерживаемый snapshot GC".into());
    }
    Ok(Some(entry))
}

fn record(entry: &LastAttempt) -> Result<(), String> {
    let _lock = MaintenanceLock::acquire().map_err(|e| e.to_string())?;
    write_private(
        &state_path()?,
        &serde_json::to_vec_pretty(entry).map_err(|e| e.to_string())?,
    )
}

/// Только кандидаты текущего Report; общий /tmp инвентаризируется, но не
/// считается проектным объектом и не попадает в список управляемых.
fn report_items(report: &Report) -> (Vec<GcItem>, usize) {
    let mut items = Vec::new();
    let target = &report.cargo_target;
    items.push(GcItem {
        category: target.category.into(),
        path: target.path.clone(),
        action: target.action.into(),
        result: target.result.into(),
        bytes_freed: target.bytes_freed,
        reason: target.reason.chars().take(200).collect(),
    });
    for c in &report.external_build_caches {
        items.push(GcItem {
            category: "external-build-cache".into(),
            path: c.path.clone(),
            action: c.action.into(),
            result: if c.bytes_freed > 0 {
                "removed"
            } else {
                "not_removed"
            }
            .into(),
            bytes_freed: c.bytes_freed,
            reason: c.reason.chars().take(200).collect(),
        });
    }
    for c in &report.project_temp {
        items.push(GcItem {
            category: c.category.into(),
            path: c.path.clone(),
            action: c.action.into(),
            result: c.result.into(),
            bytes_freed: c.bytes_freed,
            reason: c.reason.chars().take(200).collect(),
        });
    }
    let omitted = items.len().saturating_sub(MAX_REPORTED_ITEMS);
    items.truncate(MAX_REPORTED_ITEMS);
    (items, omitted)
}

pub fn record_report(report: &Report) -> Result<(), String> {
    let (items, omitted_items) = report_items(report);
    let complete = report.tmp.iter().all(|t| t.complete) && report.errors == report.fatal_errors;
    let freed = report
        .cargo_target
        .bytes_freed
        .saturating_add(report.external_cache_bytes_freed)
        .saturating_add(
            report
                .project_temp
                .iter()
                .map(|c| c.bytes_freed)
                .sum::<u64>(),
        );
    let outcome = if report.fatal_errors > 0 {
        "failed"
    } else if !complete {
        "partial"
    } else {
        "success"
    };
    record(&LastAttempt {
        schema_version: 1,
        workspace_root: report.workspace_root.clone(),
        timestamp: report.generated_unix_seconds,
        outcome: outcome.into(),
        bytes_freed: Some(freed),
        errors: report.errors,
        inventory_complete: complete,
        message: format!(
            "{}; освобождено {freed} байт; ошибок {}",
            report.mode, report.errors
        ),
        items,
        omitted_items,
    })
}

pub fn record_failure(root: &Path, message: &str) -> Result<(), String> {
    record(&LastAttempt {
        schema_version: 1,
        workspace_root: root.to_path_buf(),
        timestamp: now(),
        outcome: "failed".into(),
        bytes_freed: None,
        errors: 1,
        inventory_complete: false,
        message: message.into(),
        items: Vec::new(),
        omitted_items: 0,
    })
}

/// Необязательный графический канал. Наличие графической переменной не гарантирует D-Bus.
pub fn desktop_alert(message: &str) {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        || (std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none())
    {
        return;
    }
    let _ = Command::new("notify-send")
        .args(["--urgency=normal", "Anki-decks: обслуживание", message])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn diag(
    code: &'static str,
    severity: &'static str,
    message: impl Into<String>,
    action: Option<&'static str>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        message: message.into(),
        action,
        context: serde_json::json!({}),
    }
}

/// Диагностика пользуется тем же планировщиком и политикой, что и настоящий GC.
pub fn inspect(root: &Path) -> CareStatus {
    let mut diagnostics = Vec::new();
    let mut items = Vec::new();
    let mut omitted_items = 0;
    let preference = match read_pref() {
        Ok(p) => p.choice,
        Err(e) => {
            diagnostics.push(diag(
                "preference_unavailable",
                "error",
                e,
                Some("Проверьте права пользовательского каталога конфигурации"),
            ));
            "unknown".into()
        }
    };
    let timer = timer::inspect();
    if let Some(e) = &timer.error {
        diagnostics.push(diag(
            "timer_unavailable",
            "warning",
            e.clone(),
            Some("Проверьте systemctl --user"),
        ));
    }
    let mut automatic = timer.state.clone();
    if timer
        .installed_root
        .as_deref()
        .is_some_and(|installed| installed != root)
    {
        automatic = "other_workspace".into();
        diagnostics.push(diag(
            "timer_other_workspace",
            "warning",
            "Таймер принадлежит другому checkout; автоматическая переустановка запрещена",
            Some("Проверьте старый checkout; при необходимости вручную отключите его таймер"),
        ));
    }
    if preference == "unset" && automatic != "enabled" {
        diagnostics.push(diag(
            "setup_required",
            "info",
            "Автоматическая очистка ещё не настроена",
            Some("anki-repo maintenance enable"),
        ));
    }
    if preference == "yes" && automatic != "enabled" {
        diagnostics.push(diag(
            "preference_mismatch",
            "warning",
            "Согласие сохранено, но автоматическое обслуживание фактически не работает",
            Some("anki-repo maintenance enable"),
        ));
    }
    if preference == "no" && automatic == "enabled" {
        diagnostics.push(diag(
            "preference_mismatch",
            "warning",
            "Автоочистка работает вопреки сохранённому отказу",
            Some("anki-repo maintenance disable"),
        ));
    }
    if let Some(result) = &timer.service_result
        && result != "success"
        && result != "unknown"
    {
        diagnostics.push(diag(
            "systemd_service_failure",
            "error",
            format!("Последний статус systemd-службы: {result}"),
            Some("systemctl --user status anki-repository-maintenance.service"),
        ));
    }

    let mut measurement = Measurement {
        project_bytes: None,
        complete: false,
        cargo_target_bytes: None,
        target_warning_bytes: 0,
        target_hard_limit_bytes: 0,
        external_cache_bytes: None,
        external_cache_hard_limit_bytes: 0,
        deferred: 0,
        errors: 0,
        system_tmp: None,
    };
    match Policy::defaults() {
        Ok(policy) => {
            measurement.target_warning_bytes = policy.warning_bytes();
            measurement.target_hard_limit_bytes = policy.hard_limit_bytes();
            measurement.external_cache_hard_limit_bytes = policy.external_hard_limit_bytes();
            match run(root, &policy, &RunOptions::default()) {
                Ok(report) => {
                    (items, omitted_items) = report_items(&report);
                    measurement.complete =
                        report.tmp.iter().all(|t| t.complete) && report.errors == 0;
                    measurement.errors = report.errors;
                    measurement.system_tmp = Some(SystemTmpHealth::from_inventories(&report.tmp));
                    if report.cargo_target.ownership == "project-owned" {
                        measurement.cargo_target_bytes = Some(report.cargo_target.bytes_before);
                    }
                    measurement.external_cache_bytes = Some(report.external_cache_bytes_before);
                    measurement.project_bytes = measurement.cargo_target_bytes.map(|v| {
                        v.saturating_add(report.external_cache_bytes_before)
                            .saturating_add(
                                report
                                    .project_temp
                                    .iter()
                                    .filter(|c| c.ownership == "project-owned")
                                    .map(|c| c.bytes_before)
                                    .sum::<u64>(),
                            )
                    });
                    measurement.deferred = report
                        .project_temp
                        .iter()
                        .filter(|c| c.action == "deferred")
                        .count()
                        + report
                            .external_build_caches
                            .iter()
                            .filter(|c| c.action == "deferred")
                            .count()
                        + usize::from(report.cargo_target.action == "deferred");
                    if report.cargo_target.bytes_before >= policy.warning_bytes() {
                        diagnostics.push(diag(
                            "target_warning",
                            "warning",
                            format!(
                                "Cargo target: {} байт при пороге {} байт",
                                report.cargo_target.bytes_before,
                                policy.warning_bytes()
                            ),
                            Some("anki-repository-maintenance clean (сначала dry-run)"),
                        ));
                    }
                    if report.external_cache_bytes_before >= policy.external_hard_limit_bytes() {
                        diagnostics.push(diag(
                            "external_cache_limit",
                            "warning",
                            "Достигнут предел зарегистрированных внешних кэшей",
                            Some("anki-repository-maintenance clean (сначала dry-run)"),
                        ));
                    }
                    if let Some(tmp) = &measurement.system_tmp
                        && let Some(entry) = system_tmp_diagnostic(tmp)
                    {
                        diagnostics.push(entry);
                    }
                    if report.fatal_errors > 0 {
                        diagnostics.push(diag(
                            "gc_operation_error",
                            "error",
                            format!("Ошибка GC: {}", report.operation_errors.join("; ")),
                            Some("anki-repository-maintenance scan --json"),
                        ));
                    }
                    if measurement.deferred > 0 {
                        diagnostics.push(diag(
                            "gc_deferred",
                            "info",
                            "Часть объектов отложена согласно проверкам безопасности",
                            None,
                        ));
                    }
                }
                Err(e) => diagnostics.push(diag(
                    "measurement_unavailable",
                    "warning",
                    e,
                    Some("anki-repository-maintenance scan --json"),
                )),
            }
        }
        Err(e) => diagnostics.push(diag("policy_unavailable", "error", e, None)),
    }
    let last_attempt = match last_attempt() {
        Ok(Some(entry)) if entry.workspace_root == root => Some(entry),
        Ok(Some(_)) | Ok(None) => None,
        Err(e) => {
            diagnostics.push(diag("last_attempt_unavailable", "warning", e, None));
            None
        }
    };
    if let Some(last) = &last_attempt
        && last.outcome != "success"
    {
        diagnostics.push(diag(
            "last_gc_problem",
            if last.outcome == "partial" {
                "info"
            } else {
                "warning"
            },
            format!(
                "Последнее обслуживание: {} ({})",
                last.outcome, last.message
            ),
            Some("anki-repository-maintenance scan --json"),
        ));
    }
    CareStatus {
        schema_version: 1,
        workspace_root: root.to_path_buf(),
        preference,
        automatic,
        installed_workspace: timer.installed_root,
        timer_active: timer.active,
        measurement,
        items,
        omitted_items,
        last_attempt,
        diagnostics,
    }
}

/// Поиск именно бинарника GC: передавать исполняемый anki-repo в установщик нельзя.
fn maintenance_binary() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    if current
        .file_name()
        .is_some_and(|name| name == "anki-repository-maintenance")
    {
        return Ok(current);
    }
    let sibling = current.with_file_name("anki-repository-maintenance");
    if sibling.is_file() {
        return Ok(sibling);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("anki-repository-maintenance");
            if candidate.is_file() {
                return candidate.canonicalize().map_err(|e| e.to_string());
            }
        }
    }
    Err("не найден бинарник anki-repository-maintenance; соберите оба инструмента командой cargo build --release --workspace".into())
}

/// Согласие само по себе не означает успешную установку.
pub fn enable(root: &Path) -> Result<(), String> {
    set_choice("yes")?;
    let binary = maintenance_binary()?;
    timer::install(root, &binary)?;
    let health = timer::inspect();
    if health.state != "enabled" || health.installed_root.as_deref() != Some(root) {
        return Err(format!(
            "после установки таймер не подтверждён: {}",
            health.state
        ));
    }
    Ok(())
}

/// Не отключает unit, принадлежащий другому checkout.
pub fn disable(root: &Path) -> Result<(), String> {
    let health = timer::inspect();
    if health
        .installed_root
        .as_deref()
        .is_some_and(|installed| installed != root)
    {
        return Err(
            "таймер связан с другим checkout; отказ здесь не должен отключить чужой таймер".into(),
        );
    }
    if health.installed_root.is_some() {
        timer::uninstall_for(root)?;
    }
    set_choice("no")
}

pub fn render_text(status: &CareStatus) -> String {
    let mut s = format!(
        "Рабочая область: {}\nВыбор пользователя: {}\nАвтоочистка: {} (таймер активен: {:?})\n",
        status.workspace_root.display(),
        status.preference,
        status.automatic,
        status.timer_active
    );
    if let Some(installed) = &status.installed_workspace {
        s.push_str(&format!(
            "Установленный checkout: {}\n",
            installed.display()
        ));
    }
    let m = &status.measurement;
    s.push_str(&format!(
        "Учтённые управляемые артефакты: {} байт (полнота общего обзора, включая /tmp: {})\n",
        m.project_bytes
            .map_or_else(|| "неизвестно".into(), |v| v.to_string()),
        m.complete
    ));
    if let Some(tmp) = &m.system_tmp {
        s.push_str(&format!(
            "Общий обзор системного /tmp: полный {}, ошибок {}, границ mount {} (чужие объекты не удаляются)\n",
            tmp.complete, tmp.errors, tmp.mount_boundaries
        ));
    }
    s.push_str(&format!(
        "Cargo target: {:?} байт (предупреждение {}, предел {})\n",
        m.cargo_target_bytes, m.target_warning_bytes, m.target_hard_limit_bytes
    ));
    s.push_str(&format!(
        "Внешние кэши: {:?} байт (предел {})\n",
        m.external_cache_bytes, m.external_cache_hard_limit_bytes
    ));
    if let Some(last) = &status.last_attempt {
        s.push_str(&format!(
            "Последняя попытка: {} в {} (освобождено {:?} байт; {})\n",
            last.outcome, last.timestamp, last.bytes_freed, last.message
        ));
        for item in &last.items {
            if item.bytes_freed > 0 || item.action == "deferred" || item.action == "error" {
                s.push_str(&format!(
                    "  Результат {}: {} — {} (освобождено {} байт; {})\n",
                    item.category,
                    crate::redact_temp_namespace_uid(&item.path).display(),
                    item.result,
                    item.bytes_freed,
                    item.reason
                ));
            }
        }
        if last.omitted_items > 0 {
            s.push_str(&format!(
                "  Пропущено {} объектов; полный отчёт смотрите в журнале GC\n",
                last.omitted_items
            ));
        }
    } else {
        s.push_str("Последняя попытка для этого checkout: неизвестна\n");
    }
    for item in &status.items {
        s.push_str(&format!(
            "Текущее состояние {}: {} — {} / {} ({}); освобождено {} байт\n",
            item.category,
            crate::redact_temp_namespace_uid(&item.path).display(),
            item.action,
            item.result,
            item.reason,
            item.bytes_freed
        ));
    }
    if status.omitted_items > 0 {
        s.push_str(&format!(
            "Ещё {} объектов не выведено; используйте scan --json для полной инвентаризации\n",
            status.omitted_items
        ));
    }
    for d in &status.diagnostics {
        s.push_str(&format!("[{}] {}: {}\n", d.severity, d.code, d.message));
        if let Some(action) = d.action {
            s.push_str(&format!("  Действие: {action}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_codes_are_stable_and_rendered() {
        let d = diag(
            "setup_required",
            "info",
            "Настройка требуется",
            Some("anki-repo maintenance enable"),
        );
        assert_eq!(d.code, "setup_required");
        assert!(
            serde_json::to_string(&d)
                .unwrap()
                .contains("setup_required")
        );
    }
    #[test]
    fn private_snapshot_rejects_symlink_and_permissive_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let work = asset_store::temp_workspace::TempWorkspace::create("care-file-test").unwrap();
        let path = work.path().join("private").join("state.json");
        write_private(&path, b"{\"schema_version\":1}").unwrap();
        assert!(read_private(&path).unwrap().is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = work.path().join("state-link");
        symlink(&path, &link).unwrap();
        assert!(read_private(&link).is_err());
    }

    #[test]
    fn preference_only_accepts_explicit_choices() {
        let p = PreferenceFile {
            schema_version: 1,
            choice: "no".into(),
            last_warning_at: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let recovered: PreferenceFile = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.choice, "no");
    }

    #[test]
    fn foreign_tmp_inaccessibility_is_informational_not_gc_failure() {
        let tmp = crate::inventory::TmpInventory {
            root: PathBuf::from("/tmp"),
            allocated_bytes: 0,
            top_level_entries: 2,
            mount_boundaries: 1,
            errors: 2,
            complete: false,
            largest: Vec::new(),
        };
        let health = SystemTmpHealth::from_inventories(&[tmp]);
        assert!(!health.complete);
        assert_eq!(health.errors, 2);
        assert_eq!(health.mount_boundaries, 1);
        let diagnostic = system_tmp_diagnostic(&health).expect("partial /tmp must be described");
        assert_eq!(diagnostic.code, "system_tmp_inventory_incomplete");
        assert_eq!(diagnostic.severity, "info");
        assert_eq!(diagnostic.context["errors"], 2);
        assert_eq!(diagnostic.context["mount_boundaries"], 1);

        let complete = crate::inventory::TmpInventory {
            root: PathBuf::from("/tmp"),
            allocated_bytes: 0,
            top_level_entries: 0,
            mount_boundaries: 0,
            errors: 0,
            complete: true,
            largest: Vec::new(),
        };
        let health = SystemTmpHealth::from_inventories(&[complete]);
        assert!(system_tmp_diagnostic(&health).is_none());
    }

    /// Запускается как отдельный test process, чтобы не менять глобальное
    /// окружение остальных параллельно выполняемых тестов.
    #[test]
    fn desktop_notification_subprocess_helper() {
        if std::env::var_os("ANKI_GC_NOTIFICATION_SUBPROCESS").is_none() {
            return;
        }
        desktop_alert("ошибка GC");
    }

    #[test]
    fn desktop_notification_is_optional_and_delivery_failure_is_nonfatal() {
        use std::os::unix::fs::PermissionsExt;
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("care-notification-test").unwrap();
        let mock_bin = workspace.path().join("mock-notify-bin");
        let empty_bin = workspace.path().join("empty-notify-bin");
        std::fs::create_dir_all(&mock_bin).unwrap();
        std::fs::create_dir_all(&empty_bin).unwrap();
        let mock_notify = mock_bin.join("notify-send");
        std::fs::write(
            &mock_notify,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ANKI_GC_NOTIFICATION_LOG\"\nexit \"$ANKI_GC_NOTIFICATION_EXIT\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&mock_notify, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = workspace.path().join("notification.log");

        let invoke = |dbus: bool, gui: bool, notify_path: &Path, exit: &str| {
            let mut process = Command::new(std::env::current_exe().unwrap());
            process
                .args([
                    "--exact",
                    "care::tests::desktop_notification_subprocess_helper",
                    "--nocapture",
                ])
                .env("ANKI_GC_NOTIFICATION_SUBPROCESS", "1")
                .env("ANKI_GC_NOTIFICATION_LOG", &log)
                .env("ANKI_GC_NOTIFICATION_EXIT", exit)
                .env("PATH", notify_path)
                .env_remove("WAYLAND_DISPLAY");
            if dbus {
                process.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/absent/isolated-bus");
            } else {
                process.env_remove("DBUS_SESSION_BUS_ADDRESS");
            }
            if gui {
                process.env("DISPLAY", ":199");
            } else {
                process.env_remove("DISPLAY");
            }
            let output = process.output().unwrap();
            assert!(
                output.status.success(),
                "сбой необязательного уведомления изменил результат: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        invoke(false, false, &mock_bin, "0");
        invoke(false, true, &mock_bin, "0");
        invoke(true, false, &mock_bin, "0");
        invoke(true, true, &empty_bin, "0");
        assert!(
            !log.exists(),
            "без GUI/D-Bus/notify-send нельзя посылать уведомление"
        );

        invoke(true, true, &mock_bin, "17");
        let delivered = std::fs::read_to_string(&log).unwrap();
        assert!(
            delivered.contains("--urgency=normal Anki-decks: обслуживание ошибка GC"),
            "{delivered}"
        );
        std::fs::remove_file(&log).unwrap();
        invoke(true, true, &mock_bin, "0");
        assert!(std::fs::read_to_string(&log).unwrap().contains("ошибка GC"));
    }

    #[test]
    fn snapshot_round_trip_and_no_false_success() {
        let item = LastAttempt {
            schema_version: 1,
            workspace_root: PathBuf::from("/repo"),
            timestamp: 1,
            outcome: "partial".into(),
            bytes_freed: Some(0),
            errors: 1,
            inventory_complete: false,
            message: "неполное измерение".into(),
            items: Vec::new(),
            omitted_items: 0,
        };
        let rebuilt: LastAttempt =
            serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        assert_eq!(rebuilt.outcome, "partial");
        assert!(!rebuilt.inventory_complete);
    }
}
