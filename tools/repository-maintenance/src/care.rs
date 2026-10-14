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
    /// Метка последней фоновой проверки объёма. Отдельно от метки показанного
    /// предупреждения: интервал расходуется на саму проверку, иначе полный обход
    /// повторялся бы при каждом вызове, пока пороги не превышены.
    #[serde(default)]
    last_check_at: Option<u64>,
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
    #[serde(serialize_with = "crate::serialize_report_path")]
    pub workspace_root: PathBuf,
    pub timestamp: u64,
    pub outcome: String,
    pub bytes_freed: Option<u64>,
    pub errors: usize,
    pub inventory_complete: bool,
    #[serde(default)]
    pub managed_inventory_complete: Option<bool>,
    #[serde(default)]
    pub fatal_errors: Option<usize>,
    #[serde(default)]
    pub mode: Option<String>,
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
    pub action: Option<String>,
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
        "Общий обзор /tmp неполон; показатели подтверждённых артефактов Anki-decks учитываются отдельно. Доступные чужие каталоги могут просматриваться, но не удаляются; границы mount не пересекаются.",
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
    pub managed_complete: bool,
    pub fatal_errors: usize,
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
    #[serde(serialize_with = "crate::serialize_report_path")]
    pub workspace_root: PathBuf,
    pub preference: String,
    pub automatic: String,
    #[serde(serialize_with = "crate::serialize_optional_report_path")]
    pub installed_workspace: Option<PathBuf>,
    pub timer_active: Option<bool>,
    pub timer_enabled: Option<bool>,
    pub service_active: Option<bool>,
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
            last_check_at: None,
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
    set_choice_with_lock(choice, &_lock)
}

fn set_choice_with_lock(choice: &str, _lock: &MaintenanceLock) -> Result<(), String> {
    let mut p = read_pref()?;
    p.choice = choice.into();
    write_private(
        &pref_path()?,
        &serde_json::to_vec_pretty(&p).map_err(|e| e.to_string())?,
    )
}

/// Занимает суточный интервал фоновой проверки объёма и сообщает, нужно ли её
/// выполнять.
///
/// Метка проверки отделена от метки показанного предупреждения: интервал должен
/// расходоваться на саму проверку. Иначе у пользователя, отказавшегося от
/// автоочистки и не превышающего пороги, метка предупреждения не обновляется
/// никогда, и полный обход `/tmp`, `cargo metadata` и `systemctl` повторяется при
/// каждом интерактивном вызове.
pub fn claim_daily_check() -> Result<bool, String> {
    let _lock = MaintenanceLock::acquire().map_err(|e| e.to_string())?;
    let mut p = read_pref()?;
    if !claim_check_interval(&mut p, now()) {
        return Ok(false);
    }
    write_private(
        &pref_path()?,
        &serde_json::to_vec_pretty(&p).map_err(|e| e.to_string())?,
    )?;
    Ok(true)
}

/// Отмечает проверку в `at`, если суточный интервал истёк.
fn claim_check_interval(p: &mut PreferenceFile, at: u64) -> bool {
    if p.choice != "no"
        || p.last_check_at
            .is_some_and(|prev| at.saturating_sub(prev) < WARNING_INTERVAL)
    {
        return false;
    }
    p.last_check_at = Some(at);
    true
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

fn snapshot_bytes(entry: &LastAttempt) -> Result<Vec<u8>, String> {
    // Snapshot хранит точные рабочие пути; маскировка применяется только к
    // внешнему JSON. Это сохраняет сопоставление checkout после чтения.
    let mut stored = serde_json::to_value(entry).map_err(|e| e.to_string())?;
    stored["workspace_root"] =
        serde_json::to_value(&entry.workspace_root).map_err(|e| e.to_string())?;
    for (value, item) in stored["items"]
        .as_array_mut()
        .ok_or("snapshot: поле items не является массивом")?
        .iter_mut()
        .zip(&entry.items)
    {
        value["path"] = serde_json::to_value(&item.path).map_err(|e| e.to_string())?;
    }
    serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())
}

fn record(entry: &LastAttempt, _lock: &MaintenanceLock) -> Result<(), String> {
    let bytes = snapshot_bytes(entry)?;
    write_private(&state_path()?, &bytes)
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
    let lock = MaintenanceLock::acquire().map_err(|e| e.to_string())?;
    record_report_with_lock(report, &lock)
}

pub(crate) fn record_report_with_lock(
    report: &Report,
    lock: &MaintenanceLock,
) -> Result<(), String> {
    let (items, omitted_items) = report_items(report);
    // То же условие, что и у `measurement.complete` в `inspect`: фатальная ошибка
    // измерения `target` делает инвентаризацию неполной, а не «полной с ошибкой».
    let complete = report.tmp.iter().all(|t| t.complete) && report.errors == 0;
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
    record(
        &LastAttempt {
            schema_version: 1,
            workspace_root: report.workspace_root.clone(),
            timestamp: report.generated_unix_seconds,
            outcome: outcome.into(),
            bytes_freed: Some(freed),
            errors: report.errors,
            inventory_complete: complete,
            managed_inventory_complete: Some(managed_complete(report)),
            fatal_errors: Some(report.fatal_errors),
            mode: Some(report.mode.into()),
            message: format!("Ошибок {}", report.errors),
            items,
            omitted_items,
        },
        lock,
    )
}

pub fn record_failure(root: &Path, message: &str) -> Result<(), String> {
    let lock = MaintenanceLock::acquire().map_err(|e| e.to_string())?;
    record_failure_with_lock(root, message, &lock)
}

pub(crate) fn record_failure_with_lock(
    root: &Path,
    message: &str,
    lock: &MaintenanceLock,
) -> Result<(), String> {
    record(
        &LastAttempt {
            schema_version: 1,
            workspace_root: root.to_path_buf(),
            timestamp: now(),
            outcome: "failed".into(),
            bytes_freed: None,
            errors: 1,
            inventory_complete: false,
            managed_inventory_complete: Some(false),
            fatal_errors: Some(1),
            mode: None,
            // Ошибка парсинга конфигурации может включать длинную исходную
            // строку; снимок краткий, исходная ошибка GC возвращается целиком.
            message: message.chars().take(2000).collect(),
            items: Vec::new(),
            omitted_items: 0,
        },
        lock,
    )
}

fn managed_complete(report: &Report) -> bool {
    report.fatal_errors == 0
        && report.cargo_target.ownership == "project-owned"
        && report.external_cache_unknown_entries == 0
        && report
            .project_temp
            .iter()
            .all(|c| c.ownership == "project-owned" && c.action != "error")
}

/// Ограничивает необязательное внешнее действие; после kill обязательно wait.
fn bounded_notification(command: &mut Command, timeout: Duration) -> Result<(), String> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("не удалось запустить уведомление: {e}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "обработчик уведомления завершился с кодом {:?}",
                        status.code()
                    ))
                };
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20))
            }
            result => {
                let kill = child.kill();
                let wait = child.wait();
                return Err(format!(
                    "уведомление не завершилось за отведённое время или ожидание прервано: {result:?}; завершение: {kill:?}; ожидание: {wait:?}"
                ));
            }
        }
    }
}

/// Уведомление — необязательное побочное действие: ошибка не меняет результат GC.
pub fn desktop_alert(message: &str) {
    desktop_alert_on(is_wsl(), message);
}

/// Маршрут уведомления с явным признаком платформы: тот же код, что и в
/// `desktop_alert`, но необязательное действие остаётся необязательным.
fn desktop_alert_on(wsl: bool, message: &str) {
    let _ = try_desktop_alert_on(wsl, message);
}

/// Диагностика выбранного канала, пригодная также для безопасной проверки из systemd.
/// Успешный статус означает, что вызываемый обработчик принял запрос, но не
/// гарантирует, что пользователь увидел окно.
pub fn try_desktop_alert(message: &str) -> Result<&'static str, String> {
    try_desktop_alert_on(is_wsl(), message)
}

/// Выбор канала по уже установленной платформе. Признак передаётся параметром,
/// чтобы тест канала Linux не зависел от того, распознан ли хост как WSL по
/// `/proc` и `/mnt/c`, и не мог запустить настоящий Windows PowerShell.
fn try_desktop_alert_on(wsl: bool, message: &str) -> Result<&'static str, String> {
    if wsl {
        return windows_toast(message).map(|()| "windows-toast");
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        || (std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none())
    {
        return Err("графическая Linux-сессия с D-Bus недоступна".into());
    }
    bounded_notification(
        Command::new("notify-send").args(["--urgency=normal", "Anki-decks: обслуживание", message]),
        Duration::from_secs(5),
    )
    .map(|()| "linux-notify-send")
    .map_err(|e| format!("notify-send: {e}"))
}

/// Обнаружение WSL не опирается исключительно на WSL_INTEROP: у user systemd
/// переменной может не быть, хотя binfmt WSLInterop зарегистрирован в /proc.
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    if std::fs::read_to_string("/proc/sys/fs/binfmt_misc/WSLInterop")
        .is_ok_and(|value| value.starts_with("enabled"))
    {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|value| value.to_ascii_lowercase().contains("microsoft"))
}

/// Только постоянный PowerShell-код. Текст уведомления передаётся строковым
/// литералом с экранированными апострофами — без shell и PowerShell-инъекций.
fn power_shell_script(message: &str) -> String {
    let escaped = message.replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'Stop';          Import-Module BurntToast -ErrorAction Stop;          New-BurntToastNotification -Text @('Anki-decks: обслуживание', '{escaped}') -ErrorAction Stop"
    )
}

// Путь PATH имеет приоритет над резервом, в том числе в изолированных тестах.
fn powershell_executable(
    path: Option<std::ffi::OsString>,
    automount: &Path,
) -> Result<PathBuf, String> {
    let from_path = path.and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("powershell.exe"))
            .find(|p| p.is_file())
    });
    from_path.or_else(|| {
        let candidate = automount.join("c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        candidate.is_file().then_some(candidate)
    }).ok_or("powershell.exe не найден в PATH или Windows automount; проверьте WSL interop и добавьте Windows PowerShell в PATH службы".into())
}

fn windows_toast(message: &str) -> Result<(), String> {
    // Сохраняем стандартный /mnt/c; нестандартный automount читаем из wsl.conf.
    let config = fs::read_to_string("/etc/wsl.conf").unwrap_or_default();
    let automount = wsl_automount(&config);
    let executable = powershell_executable(std::env::var_os("PATH"), &automount)?;
    bounded_notification(
        Command::new(executable).args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &power_shell_script(message),
        ]),
        Duration::from_secs(5),
    )
    .map_err(|e| format!("Windows PowerShell/BurntToast: {e}"))
}

fn wsl_automount(config: &str) -> PathBuf {
    let mut in_section = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[automount]");
        }
        if in_section
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("root")
        {
            let value = value.trim().trim_matches('"');
            if Path::new(value).is_absolute() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from("/mnt")
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
        action: action.map(str::to_owned),
        context: serde_json::json!({}),
    }
}

/// Диагностика пользуется тем же планировщиком и политикой, что и настоящий GC.
pub fn inspect(root: &Path) -> CareStatus {
    inspect_with_policy(root, Policy::defaults(), &RunOptions::default())
}

fn inspect_with_policy(
    root: &Path,
    policy: Result<Policy, String>,
    options: &RunOptions,
) -> CareStatus {
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
    if preference == "yes"
        && (automatic != "enabled" || timer.enabled != Some(true) || timer.active != Some(true))
    {
        diagnostics.push(diag(
            "preference_mismatch",
            "warning",
            "Согласие сохранено, но автоматическое обслуживание фактически не работает",
            Some("anki-repo maintenance enable"),
        ));
    }
    if preference == "no"
        && (timer.enabled == Some(true)
            || timer.active == Some(true)
            || timer.service_active == Some(true))
    {
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
        managed_complete: false,
        fatal_errors: 0,
        cargo_target_bytes: None,
        target_warning_bytes: 0,
        target_hard_limit_bytes: 0,
        external_cache_bytes: None,
        external_cache_hard_limit_bytes: 0,
        deferred: 0,
        errors: 0,
        system_tmp: None,
    };
    match policy {
        Ok(policy) => {
            measurement.target_warning_bytes = policy.warning_bytes();
            measurement.target_hard_limit_bytes = policy.hard_limit_bytes();
            measurement.external_cache_hard_limit_bytes = policy.external_hard_limit_bytes();
            match run(root, &policy, options) {
                Ok(report) => {
                    (items, omitted_items) = report_items(&report);
                    measurement.complete =
                        report.tmp.iter().all(|t| t.complete) && report.errors == 0;
                    measurement.errors = report.errors;
                    measurement.managed_complete = managed_complete(&report);
                    measurement.fatal_errors = report.fatal_errors;
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
                            Some("anki-repository-maintenance clean"),
                        ));
                    }
                    if report.external_cache_bytes_before >= policy.external_hard_limit_bytes() {
                        diagnostics.push(diag(
                            "external_cache_limit",
                            "warning",
                            "Достигнут предел зарегистрированных внешних кэшей",
                            Some("anki-repository-maintenance clean"),
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
                "Последнее обслуживание: {}; ошибок {}",
                outcome_text(&last.outcome),
                last.errors
            ),
            Some("anki-repository-maintenance scan --json"),
        ));
    }
    // Все исполняемые рекомендации привязаны к диагностируемому checkout.
    let quoted = root
        .to_str()
        .map(|root| format!("'{}'", root.replace('\'', "'\\''")));
    for diagnostic in &mut diagnostics {
        if let Some(action) = &mut diagnostic.action {
            // JSON-диагностика может показать lossy-путь, но исполняемая команда
            // обязана использовать точные байты checkout.
            let needs_root = action.starts_with("anki-repository-maintenance ")
                || action.starts_with("anki-repo maintenance ");
            if needs_root && quoted.is_none() {
                diagnostic.action = None;
            }
        }
        if let (Some(action), Some(quoted)) = (&mut diagnostic.action, &quoted) {
            if let Some(tail) = action.strip_prefix("anki-repository-maintenance ") {
                let (verb, args) = tail.split_once(' ').unwrap_or((tail, ""));
                *action =
                    format!("anki-repository-maintenance {verb} --workspace-root {quoted} {args}")
                        .trim_end()
                        .into();
            } else if let Some(tail) = action.strip_prefix("anki-repo maintenance ") {
                *action = format!("anki-repo maintenance --workspace-root {quoted} {tail}");
            }
        }
        if matches!(
            diagnostic.code,
            "timer_unavailable"
                | "timer_other_workspace"
                | "preference_mismatch"
                | "systemd_service_failure"
        ) {
            diagnostic.context["timer_enabled"] = serde_json::json!(timer.enabled);
            diagnostic.context["timer_active"] = serde_json::json!(timer.active);
            diagnostic.context["service_active"] = serde_json::json!(timer.service_active);
            diagnostic.context["installed_workspace"] =
                serde_json::json!(timer.installed_root.as_deref().map(|path| {
                    crate::redact_temp_namespace_uid(path)
                        .to_string_lossy()
                        .into_owned()
                }));
        }
        if diagnostic.code == "last_gc_problem"
            && let Some(last) = &last_attempt
        {
            diagnostic.context["outcome"] = serde_json::json!(last.outcome);
            diagnostic.context["mode"] = serde_json::json!(last.mode);
            diagnostic.context["message"] = serde_json::json!(last.message);
        }
        diagnostic.context["workspace_root"] =
            serde_json::json!(crate::redact_temp_namespace_uid(root).to_string_lossy());
    }
    CareStatus {
        schema_version: 1,
        workspace_root: root.to_path_buf(),
        preference,
        automatic,
        installed_workspace: timer.installed_root,
        timer_active: timer.active,
        timer_enabled: timer.enabled,
        service_active: timer.service_active,
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
    let lock = MaintenanceLock::acquire().map_err(|e| format!("блокировка настройки GC: {e}"))?;
    // Согласие намеренно сохраняется при неудачной установке: повторный enable
    // восстанавливает её. Ошибка возвращается вызывающему коду явно.
    set_choice_with_lock("yes", &lock)?;
    let binary = maintenance_binary()?;
    timer::install_with_lock(root, &binary, &lock)?;
    let health = timer::inspect();
    if health.state != "enabled"
        || health.enabled != Some(true)
        || health.active != Some(true)
        || health.installed_root.as_deref() != Some(root)
    {
        return Err(format!(
            "после установки таймер не подтверждён: {}; {:?}",
            health.state, health.error
        ));
    }
    Ok(())
}

/// Проверка фактов, отключение и сохранение отказа составляют одну операцию.
pub fn disable(root: &Path) -> Result<(), String> {
    let lock = MaintenanceLock::acquire().map_err(|e| format!("блокировка настройки GC: {e}"))?;
    let health = timer::inspect();
    if matches!(health.state.as_str(), "unknown" | "unavailable") {
        return Err(format!(
            "таймер не подтверждён; отключение запрещено: {:?}",
            health.error
        ));
    }
    if health
        .installed_root
        .as_deref()
        .is_some_and(|installed| installed != root)
    {
        return Err("таймер связан с другим checkout; отключение запрещено".into());
    }
    if health.installed_root.is_some() {
        timer::uninstall_with_lock(Some(root), &lock)?;
    } else if timer::artifacts_present()? {
        return Err("принадлежность артефактов таймера не подтверждена".into());
    }
    let after = timer::inspect();
    if after.state != "disabled"
        || after.enabled != Some(false)
        || after.active != Some(false)
        || after.service_active != Some(false)
        || timer::artifacts_present()?
    {
        return Err(format!(
            "автоматическая очистка не отключена: {}; {:?}",
            after.state, after.error
        ));
    }
    set_choice_with_lock("no", &lock)
}

/// Значение необязательного счётчика в пользовательском тексте: `None` — это
/// «неизвестно», а не внутреннее представление Rust `None`.
fn bytes_or_unknown(value: Option<u64>) -> String {
    value.map_or_else(|| "неизвестно".into(), |value| value.to_string())
}

/// Состояние необязательного признака в пользовательском тексте.
fn active_or_unknown(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "да",
        Some(false) => "нет",
        None => "неизвестно",
    }
}

fn outcome_text(value: &str) -> &str {
    match value {
        "success" => "успешно",
        "partial" => "неполный обзор",
        "failed" => "сбой",
        _ => "неизвестно",
    }
}
fn mode_text(value: Option<&str>) -> &str {
    match value {
        Some("apply") => "очистка с записью",
        Some("scan") => "обзор",
        Some("dry-run") => "предварительный просмотр",
        _ => "неизвестно",
    }
}
fn age_text(timestamp: u64, at: u64) -> String {
    if timestamp > at {
        return "время из будущего (проверьте часы)".into();
    }
    let age = at - timestamp;
    if age < 60 {
        "менее минуты назад".into()
    } else if age < 3600 {
        format!("{} мин. назад", age / 60)
    } else if age < 86400 {
        format!("{} ч. назад", age / 3600)
    } else {
        format!("{} дн. назад", age / 86400)
    }
}

pub fn render_text(status: &CareStatus) -> String {
    let mut s = format!(
        "Рабочая область: {}\nВыбор пользователя: {}\nАвтоочистка: {} (таймер активен: {})\n",
        status.workspace_root.display(),
        match status.preference.as_str() {
            "yes" => "согласие",
            "no" => "отказ",
            "unset" => "ещё не выбран",
            _ => "неизвестно",
        },
        match status.automatic.as_str() {
            "enabled" => "включена",
            "disabled" => "отключена",
            "other_workspace" => "принадлежит другому checkout",
            "unavailable" => "systemd недоступен",
            _ => "неизвестно",
        },
        active_or_unknown(status.timer_active)
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
        bytes_or_unknown(m.project_bytes),
        active_or_unknown(Some(m.complete))
    ));
    if let Some(tmp) = &m.system_tmp {
        s.push_str(&format!(
            "Общий обзор системного /tmp: полный {}, ошибок {}, границ mount {} (чужие объекты не удаляются)\n",
            active_or_unknown(Some(tmp.complete)), tmp.errors, tmp.mount_boundaries
        ));
    }
    s.push_str(&format!(
        "Cargo target: {} байт (предупреждение {}, предел {})\n",
        bytes_or_unknown(m.cargo_target_bytes),
        m.target_warning_bytes,
        m.target_hard_limit_bytes
    ));
    s.push_str(&format!(
        "Внешние кэши: {} байт (предел {})\n",
        bytes_or_unknown(m.external_cache_bytes),
        m.external_cache_hard_limit_bytes
    ));
    if let Some(last) = &status.last_attempt {
        s.push_str(&format!(
            "Последняя попытка: {}, {} (режим: {}; освобождено {} байт; ошибок {})\n",
            outcome_text(&last.outcome),
            age_text(last.timestamp, now()),
            mode_text(last.mode.as_deref()),
            bytes_or_unknown(last.bytes_freed),
            last.errors
        ));
        if last.outcome == "failed" && last.bytes_freed.is_none() {
            s.push_str(&format!("Причина сбоя: {}\n", last.message));
        }
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
        if let Some(action) = &d.action {
            s.push_str(&format!("  Действие: {action}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_preserves_exact_utf8_paths_and_rejects_non_utf8_roots_and_items() {
        use std::os::unix::ffi::OsStringExt;
        let root = PathBuf::from("/tmp/anki-decks-123/checkout");
        let mut entry = LastAttempt {
            schema_version: 1,
            workspace_root: root.clone(),
            timestamp: 1,
            outcome: "success".into(),
            bytes_freed: Some(0),
            errors: 0,
            inventory_complete: true,
            managed_inventory_complete: Some(true),
            fatal_errors: Some(0),
            mode: Some("scan".into()),
            message: "проверка".into(),
            items: vec![GcItem {
                category: "target".into(),
                path: root.join("target"),
                action: "keep".into(),
                result: "kept".into(),
                bytes_freed: 0,
                reason: "проверка".into(),
            }],
            omitted_items: 0,
        };
        let stored: LastAttempt = serde_json::from_slice(&snapshot_bytes(&entry).unwrap()).unwrap();
        assert_eq!(stored.workspace_root, root);
        assert_eq!(stored.items[0].path, root.join("target"));
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/path-\xff".to_vec()));
        entry.workspace_root = invalid.clone();
        assert!(snapshot_bytes(&entry).unwrap_err().contains("UTF-8"));
        entry.workspace_root = root;
        entry.items[0].path = invalid;
        assert!(snapshot_bytes(&entry).unwrap_err().contains("UTF-8"));
    }

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
            last_check_at: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let recovered: PreferenceFile = serde_json::from_str(&json).unwrap();
        assert_eq!(recovered.choice, "no");
    }

    #[test]
    fn preference_file_without_check_marker_is_still_readable() {
        let older = br#"{"schema_version":1,"choice":"no","last_warning_at":null}"#;
        let p: PreferenceFile = serde_json::from_slice(older).unwrap();
        assert_eq!(p.choice, "no");
        assert_eq!(p.last_check_at, None);
    }

    #[test]
    fn daily_check_interval_is_claimed_once_per_day() {
        let mut p = PreferenceFile {
            schema_version: 1,
            choice: "no".into(),
            last_warning_at: None,
            last_check_at: None,
        };
        assert!(claim_check_interval(&mut p, 1_000));
        assert!(!claim_check_interval(&mut p, 1_000 + WARNING_INTERVAL - 1));
        assert!(claim_check_interval(&mut p, 1_000 + WARNING_INTERVAL));
        assert_eq!(
            p.last_warning_at, None,
            "проверка не должна расходовать метку показанного предупреждения"
        );
    }

    #[test]
    fn daily_check_is_not_claimed_without_a_saved_refusal() {
        let mut p = PreferenceFile {
            schema_version: 1,
            choice: "unset".into(),
            last_warning_at: None,
            last_check_at: None,
        };
        assert!(!claim_check_interval(&mut p, 1_000));
        assert_eq!(p.last_check_at, None);
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
        let diagnostic =
            system_tmp_diagnostic(&health).expect("неполный обзор /tmp должен быть описан");
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

    /// Запускается как отдельный тестового процесса, чтобы не менять глобальное
    /// окружение остальных параллельно выполняемых тестов. Канал выбирается
    /// параметром, а не распознаванием хоста: на WSL-хосте `is_wsl()` вернул бы
    /// true и проверка Linux-маршрута обратилась бы к настоящему Windows PowerShell.
    #[test]
    fn desktop_notification_subprocess_helper() {
        if std::env::var_os("ANKI_GC_NOTIFICATION_SUBPROCESS").is_none() {
            return;
        }
        desktop_alert_on(false, "ошибка GC");
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

    /// Запускаем независимым процессом, чтобы окружение теста не влияло на
    /// остальные тесты Cargo, и проверяем маршрут WSL без Windows/GUI.
    #[test]
    fn windows_notification_subprocess_helper() {
        if std::env::var_os("ANKI_GC_WINDOWS_NOTIFICATION_TEST").is_none() {
            return;
        }
        let message = "ошибка GC: O'Brien; $(rm -rf /)";
        assert_eq!(try_desktop_alert(message).unwrap(), "windows-toast");
    }

    #[test]
    fn windows_notification_is_escaped_and_works_without_gui_or_wsl_interop_env() {
        use std::os::unix::fs::PermissionsExt;
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("care-wsl-notification").unwrap();
        let bin = workspace.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = workspace.path().join("powershell-args.txt");
        let fake = bin.join("powershell.exe");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$ANKI_GC_WINDOWS_NOTIFICATION_LOG\"\nexit \"$ANKI_GC_WINDOWS_NOTIFICATION_EXIT\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let invoke = |exit_code: &str| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "care::tests::windows_notification_subprocess_helper",
                    "--nocapture",
                ])
                .env("ANKI_GC_WINDOWS_NOTIFICATION_TEST", "1")
                .env("ANKI_GC_WINDOWS_NOTIFICATION_LOG", &log)
                .env("ANKI_GC_WINDOWS_NOTIFICATION_EXIT", exit_code)
                .env("WSL_DISTRO_NAME", "synthetic-wsl")
                .env_remove("WSL_INTEROP")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env("PATH", &bin)
                .output()
                .unwrap()
        };
        let success = invoke("0");
        assert!(
            success.status.success(),
            "{}",
            String::from_utf8_lossy(&success.stderr)
        );
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(
            args.contains("-NoProfile -NonInteractive -Command"),
            "{args}"
        );
        assert!(args.contains("O''Brien; $(rm -rf /)"), "{args}");
        assert!(args.contains("New-BurntToastNotification"), "{args}");
        assert!(!args.contains("O'Brien;"), "{args}");
        assert!(!args.contains("notify-send"), "{args}");

        let failed = invoke("17");
        assert!(
            !failed.status.success(),
            "проверочный try_desktop_alert должен сообщать сбой доставки"
        );
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains(
                "Windows PowerShell/BurntToast: обработчик уведомления завершился с кодом"
            ),
            "{}",
            String::from_utf8_lossy(&failed.stderr)
        );
    }

    #[test]
    fn powershell_escaping_handles_quotes_and_multiline_without_command_breakout() {
        let text = power_shell_script("пример: 'строка'\\n; Remove-Item /");
        assert!(text.contains("пример: ''строка''\\n; Remove-Item /"));
        assert!(text.contains("New-BurntToastNotification"));
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
            managed_inventory_complete: None,
            fatal_errors: None,
            mode: None,
            message: "неполное измерение".into(),
            items: Vec::new(),
            omitted_items: 0,
        };
        let rebuilt: LastAttempt =
            serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        assert_eq!(rebuilt.outcome, "partial");
        assert!(!rebuilt.inventory_complete);
    }
    #[test]
    fn notification_timeout_helper() {
        let Ok(channel) = std::env::var("CARE_TIMEOUT_CHANNEL") else {
            return;
        };
        let started = std::time::Instant::now();
        let error = try_desktop_alert_on(channel == "windows", "проверка таймаута").unwrap_err();
        assert!(error.contains("отведённое время"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn both_notification_channels_timeout_and_reap_child() {
        let owner =
            asset_store::temp_workspace::TempWorkspace::create("notification-timeout").unwrap();
        let bin = owner.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["notify-send", "powershell.exe"] {
            let path = bin.join(name);
            fs::write(
                &path,
                "#!/bin/sh\nprintf '%s' \"$$\" > \"$CARE_NOTIFICATION_PID\"\nexec /bin/sleep 60\n",
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut children = Vec::new();
        for channel in ["linux", "windows"] {
            let pid = owner.path().join(channel);
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "care::tests::notification_timeout_helper",
                    "--nocapture",
                ])
                .env_clear()
                .env("HOME", owner.path())
                .env("PATH", &bin)
                .env("CARE_TIMEOUT_CHANNEL", channel)
                .env("CARE_NOTIFICATION_PID", &pid)
                .env("DISPLAY", ":199")
                .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/isolated")
                .spawn()
                .unwrap();
            children.push((child, pid));
        }
        for (mut child, pidfile) in children {
            assert!(child.wait().unwrap().success());
            let pid = fs::read_to_string(pidfile).unwrap();
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "дочерний процесс должен быть завершён и собран"
            );
        }
    }

    #[test]
    fn powershell_discovery_keeps_path_priority_and_handles_custom_mount() {
        let owner =
            asset_store::temp_workspace::TempWorkspace::create("powershell-discovery").unwrap();
        let bin = owner.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let custom = owner.path().join("custom-mount");
        let fallback = custom.join("c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        fs::write(&fallback, "").unwrap();
        let stub = bin.join("powershell.exe");
        fs::write(&stub, "").unwrap();
        assert_eq!(
            powershell_executable(Some(bin.clone().into_os_string()), &custom).unwrap(),
            stub
        );
        fs::remove_file(stub).unwrap();
        assert_eq!(
            powershell_executable(Some(bin.into_os_string()), &custom).unwrap(),
            fallback
        );
        fs::remove_file(fallback).unwrap();
        assert!(
            powershell_executable(None, &custom)
                .unwrap_err()
                .contains("PATH службы")
        );
        let config = format!(
            "[boot]\nsystemd=true\n[automount]\nroot = {}\n",
            custom.display()
        );
        assert_eq!(wsl_automount(&config), custom);
        assert_eq!(wsl_automount(""), Path::new("/mnt"));
    }

    #[test]
    fn diagnostic_policy_conditions_helper() {
        let Ok(scenario) = std::env::var("CARE_POLICY_SCENARIO") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("CARE_ROOT").unwrap());
        let mut policy = Policy::defaults().unwrap();
        policy.target_warning_bytes = Some(1);
        policy.target_hard_limit_bytes = Some(1024 * 1024);
        policy.external_cache_hard_limit_bytes = Some(1);
        policy.external_cache_goal_bytes = Some(0);
        let tmp = std::env::temp_dir();
        let live = asset_store::temp_workspace::TempWorkspace::create_under(&tmp, "care-live-temp")
            .unwrap();
        fs::write(live.path().join("data"), [1; 4096]).unwrap();
        let cache = crate::policy::cache_home()
            .unwrap()
            .join("anki-decks/repository-maintenance");
        let cache_dir = crate::cache::initialize_build_cache(&root, &cache, "diagnostic").unwrap();
        fs::create_dir_all(cache_dir.join("target")).unwrap();
        fs::write(
            cache_dir.join("target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        fs::write(cache_dir.join("target/data"), [2; 4096]).unwrap();
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data"), [3; 4096]).unwrap();
        let closed = tmp.join("foreign-private");
        fs::create_dir_all(&closed).unwrap();
        fs::write(closed.join("data"), [4; 4096]).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if scenario == "unsafe" {
            fs::remove_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&cache_dir, &target).unwrap();
        }
        let options = RunOptions {
            temp_root: tmp,
            ..Default::default()
        };
        let status = inspect_with_policy(
            &root,
            if scenario == "policy-error" {
                Err("проверяемая ошибка политики".into())
            } else {
                Ok(policy)
            },
            &options,
        );
        fs::set_permissions(closed, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            std::env::var_os("CARE_OUTPUT").unwrap(),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn diagnostic_thresholds_deferred_partial_inventory_and_policy_error_use_real_inspect() {
        let owner =
            asset_store::temp_workspace::TempWorkspace::create("care-policy-diagnostics").unwrap();
        for scenario in ["thresholds", "unsafe", "policy-error"] {
            let base = owner.path().join(scenario);
            let root = base.join("checkout with spaces");
            for name in ["bin", "home", "config", "state", "cache", "tmp"] {
                fs::create_dir_all(base.join(name)).unwrap();
            }
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
            let cargo = base.join("bin/cargo");
            fs::write(&cargo,"#!/bin/sh\nprintf '{\"workspace_root\":\"%s\",\"target_directory\":\"%s/target\"}\\n' \"$CARE_ROOT\" \"$CARE_ROOT\"\n").unwrap();
            fs::set_permissions(cargo, fs::Permissions::from_mode(0o755)).unwrap();
            // Недоступный user-systemd также является реальным условием диагностики.
            let systemctl = base.join("bin/systemctl");
            fs::write(&systemctl, "#!/bin/sh\nexit 5\n").unwrap();
            fs::set_permissions(systemctl, fs::Permissions::from_mode(0o755)).unwrap();
            let output = base.join("out.json");
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "care::tests::diagnostic_policy_conditions_helper",
                    "--nocapture",
                ])
                .env_clear()
                .env("HOME", base.join("home"))
                .env("XDG_CONFIG_HOME", base.join("config"))
                .env("XDG_STATE_HOME", base.join("state"))
                .env("XDG_CACHE_HOME", base.join("cache"))
                .env("TMPDIR", base.join("tmp"))
                .env("PATH", base.join("bin"))
                .env("CARE_ROOT", &root)
                .env("CARE_OUTPUT", &output)
                .env("CARE_POLICY_SCENARIO", scenario)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
            let diagnostics = value["diagnostics"].as_array().unwrap();
            let expected = match scenario {
                "thresholds" => vec![
                    ("target_warning", "warning"),
                    ("external_cache_limit", "warning"),
                    ("gc_deferred", "info"),
                    ("system_tmp_inventory_incomplete", "info"),
                ],
                "unsafe" => vec![("gc_operation_error", "error")],
                _ => vec![("policy_unavailable", "error")],
            };
            for (code, severity) in expected {
                if code == "system_tmp_inventory_incomplete"
                    && fs::metadata("/proc/self").unwrap().uid() == 0
                {
                    continue;
                }
                let d = diagnostics
                    .iter()
                    .find(|d| d["code"] == code)
                    .unwrap_or_else(|| panic!("нет {code}: {value}"));
                assert_eq!(d["severity"], severity);
                assert!(d["context"]["workspace_root"].is_string());
                if matches!(code, "policy_unavailable" | "gc_deferred") {
                    assert!(d["action"].is_null());
                }
                if matches!(
                    code,
                    "system_tmp_inventory_incomplete" | "gc_operation_error"
                ) {
                    assert!(
                        d["action"]
                            .as_str()
                            .unwrap()
                            .starts_with("anki-repository-maintenance scan --workspace-root")
                    );
                }
                if code == "target_warning" || code == "external_cache_limit" {
                    let action = d["action"].as_str().unwrap();
                    assert!(action.contains("--workspace-root"));
                    assert!(action.contains("checkout with spaces"));
                    assert!(!action.contains("--apply"));
                    assert!(!action.contains("(сначала"));
                }
            }
            if scenario == "thresholds" {
                assert!(
                    !diagnostics
                        .iter()
                        .any(|d| d["code"] == "gc_operation_error")
                );
                assert_eq!(value["measurement"]["fatal_errors"], 0);
                assert_eq!(value["measurement"]["managed_complete"], true);
            }
        }
    }

    #[test]
    fn readable_status_localizes_values_without_changing_json() {
        let status = CareStatus {
            schema_version: 1,
            workspace_root: PathBuf::from("/repo"),
            preference: "yes".into(),
            automatic: "enabled".into(),
            installed_workspace: None,
            timer_active: Some(true),
            timer_enabled: Some(true),
            service_active: Some(false),
            measurement: Measurement {
                project_bytes: None,
                complete: false,
                managed_complete: false,
                fatal_errors: 0,
                cargo_target_bytes: None,
                target_warning_bytes: 1,
                target_hard_limit_bytes: 2,
                external_cache_bytes: None,
                external_cache_hard_limit_bytes: 1,
                deferred: 0,
                errors: 0,
                system_tmp: Some(SystemTmpHealth {
                    complete: false,
                    errors: 1,
                    mount_boundaries: 0,
                }),
            },
            items: vec![],
            omitted_items: 0,
            last_attempt: None,
            diagnostics: vec![],
        };
        let text = render_text(&status);
        assert!(text.contains("неизвестна"));
        assert!(!text.contains("true"));
        assert!(!text.contains("false"));
        for outcome in ["success", "partial", "failed"] {
            let mut s = status.clone();
            s.last_attempt = Some(LastAttempt {
                schema_version: 1,
                workspace_root: PathBuf::from("/repo"),
                timestamp: now() - 120,
                outcome: outcome.into(),
                bytes_freed: Some(123),
                errors: 0,
                inventory_complete: false,
                managed_inventory_complete: None,
                fatal_errors: None,
                mode: Some("apply".into()),
                message: "apply; освобождено 123 байт; ошибок 0".into(),
                items: vec![],
                omitted_items: 0,
            });
            let json = serde_json::to_value(&s).unwrap();
            let text = render_text(&s);
            assert_eq!(text.matches("освобождено 123 байт").count(), 1);
            assert!(!text.contains("apply"));
            assert!(!text.contains("partial"));
            assert!(text.contains("мин. назад"));
            assert_eq!(json["last_attempt"]["mode"], "apply");
            assert_eq!(json["last_attempt"]["outcome"], outcome);
            assert_eq!(json, serde_json::to_value(&s).unwrap());
        }
        assert!(age_text(now() + 10, now()).contains("будущего"));
    }
}
