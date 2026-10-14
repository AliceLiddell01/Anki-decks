use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::policy::config_home;

const SERVICE_NAME: &str = "anki-repository-maintenance.service";
const TIMER_NAME: &str = "anki-repository-maintenance.timer";
pub(crate) const INSTALLED_CONFIG_NAME: &str = "repository-maintenance-install.toml";
const SERVICE_TEMPLATE: &str = include_str!("../systemd/anki-repository-maintenance.service");
const TIMER_TEMPLATE: &str = include_str!("../systemd/anki-repository-maintenance.timer");
// Точная подпись владения служебным файлом systemd, которую установщик проверяет при обновлении и удалении.
const MANAGED_MARKER: &str = "# Managed by repository-maintenance; do not edit by hand.";
const CARGO_PATH_MARKER: &str = "@CARGO_PATH_ENV@";

#[derive(Debug, Clone, Serialize)]
pub struct TimerResult {
    pub state: &'static str,
    pub message: String,
    pub command_output: Option<String>,
}

#[derive(Serialize)]
struct InstalledConfig<'a> {
    managed_by: &'static str,
    schema: u32,
    repository_root: &'a Path,
}

#[derive(Debug, Clone)]
pub struct TimerInspection {
    pub state: String,
    pub active: Option<bool>,
    pub enabled: Option<bool>,
    pub service_active: Option<bool>,
    pub installed_root: Option<PathBuf>,
    pub service_result: Option<String>,
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledConfigOwned {
    managed_by: String,
    schema: u32,
    repository_root: PathBuf,
}

pub(crate) fn installed_root() -> Result<Option<PathBuf>, String> {
    let home = config_home().ok_or("не задан HOME / XDG_CONFIG_HOME")?;
    let path = home.join(format!("anki-decks/{INSTALLED_CONFIG_NAME}"));
    let meta = match fs::symlink_metadata(&path) {
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
    {
        return Err(format!(
            "небезопасная конфигурация установки {}",
            path.display()
        ));
    }
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let config: InstalledConfigOwned = toml::from_str(&text).map_err(|e| e.to_string())?;
    if config.managed_by != "repository-maintenance" || config.schema != 1 {
        return Err("неподдерживаемая конфигурация установки таймера".into());
    }
    Ok(Some(config.repository_root))
}

/// Пути установки принадлежат таймеру, а не потребителям диагностики.
pub(crate) fn artifact_paths() -> Result<[PathBuf; 3], String> {
    let home = config_home().ok_or("не задан HOME / XDG_CONFIG_HOME")?;
    Ok([
        home.join("systemd/user").join(SERVICE_NAME),
        home.join("systemd/user").join(TIMER_NAME),
        home.join("anki-decks").join(INSTALLED_CONFIG_NAME),
    ])
}

pub(crate) fn artifacts_present() -> Result<bool, String> {
    for path in artifact_paths()? {
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(false)
}

fn installed_binary() -> Result<PathBuf, String> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("не задан HOME")?)
            .join(".local/bin/anki-repository-maintenance"),
    )
}

// Ненулевой exit code сам по себе не доказывает выключенный таймер.
fn query_flag(verb: &str, absent: bool) -> Result<bool, String> {
    let out = Command::new("systemctl")
        .args(["--user", verb, TIMER_NAME])
        .output()
        .map_err(|e| e.to_string())?;
    let value = String::from_utf8_lossy(&out.stdout);
    match (verb, value.trim(), out.status.code()) {
        ("is-enabled", "enabled" | "enabled-runtime", Some(0)) => Ok(true),
        ("is-enabled", "disabled" | "masked" | "masked-runtime", Some(1)) => Ok(false),
        ("is-enabled", "static" | "indirect", Some(0)) => Ok(false),
        ("is-enabled", "", Some(1 | 4)) if absent => Ok(false),
        ("is-enabled", "not-found", Some(4)) => Ok(false),
        ("is-active", "active" | "activating" | "reloading", Some(0)) => Ok(true),
        ("is-active", "inactive" | "failed", Some(3)) => Ok(false),
        ("is-active", "unknown", Some(4)) => Ok(false),
        _ => Err(format!(
            "неизвестный ответ systemctl {verb}: {} (код {:?})",
            value.trim(),
            out.status.code()
        )),
    }
}

fn loaded_unit(name: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    let text = systemctl(&[
        "show",
        name,
        "--property=LoadState,FragmentPath,ExecStart,ActiveState,Result,DropInPaths,NeedDaemonReload",
    ])?;
    let properties: std::collections::BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    if !properties.contains_key("LoadState") || !properties.contains_key("ActiveState") {
        return Err(format!("неполный ответ systemctl show для {name}"));
    }
    Ok(properties)
}

fn verify_unit(
    name: &str,
    path: &Path,
    loaded: &std::collections::BTreeMap<String, String>,
    allow_missing_binary: bool,
) -> Result<(), String> {
    let get = |key: &str| loaded.get(key).map(String::as_str).unwrap_or("");
    let unloaded = get("LoadState") == "not-found"
        && get("FragmentPath").is_empty()
        && matches!(get("ActiveState"), "inactive" | "failed")
        && get("DropInPaths").is_empty();
    if get("LoadState") == "not-found" && !unloaded {
        return Err(format!("противоречивое состояние незагруженного {name}"));
    }
    if unloaded {
        // Файлы на диске всё равно проверяются перед переустановкой/удалением.
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
            Ok(_) => (),
        }
    }
    if !unloaded
        && (get("LoadState") != "loaded"
            || Path::new(get("FragmentPath")) != path
            || !get("DropInPaths").is_empty()
            || get("NeedDaemonReload") != "no")
    {
        return Err(format!(
            "загруженный {name} не соответствует управляемому unit (FragmentPath={}, LoadState={})",
            get("FragmentPath"),
            get("LoadState")
        ));
    }
    let directory = fs::symlink_metadata(path.parent().ok_or("нет каталога unit")?)
        .map_err(|e| e.to_string())?;
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    if directory.file_type().is_symlink()
        || !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o022 != 0
    {
        return Err("каталог управляемых unit небезопасен".into());
    }
    ensure_managed(path)?;
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    if name == TIMER_NAME {
        if text != format!("{MANAGED_MARKER}\n{TIMER_TEMPLATE}") {
            return Err("содержимое управляемого таймера изменено".into());
        }
    } else {
        // Проверяем весь шаблон, разрешая только подстановку PATH установщиком.
        let lines: Vec<_> = text
            .lines()
            .filter(|l| !l.starts_with("Environment=\"PATH="))
            .collect();
        let expected: Vec<_> = SERVICE_TEMPLATE
            .lines()
            .filter(|l| *l != CARGO_PATH_MARKER)
            .collect();
        if lines.first() != Some(&MANAGED_MARKER) || lines[1..] != expected {
            return Err("содержимое управляемой службы изменено".into());
        }
        let binary = installed_binary()?;
        ensure_binary_managed_or_absent(&binary, path, &artifact_paths()?[2])?;
        if !allow_missing_binary && !binary.is_file() {
            return Err("установленный бинарник отсутствует".into());
        }
        if unloaded {
            return Ok(());
        }
        let exec = get("ExecStart");
        // show использует структурированную запись с path и argv[]. Дополнительные
        // команды, аргументы и исполняемые файлы нельзя считать нашей службой.
        let expected_path = format!("path={} ;", binary.display());
        let expected_args = format!(
            "argv[]={} clean --apply --installed-root ;",
            binary.display()
        );
        if exec.matches("path=").count() != 1
            || !exec.contains(&expected_path)
            || !exec.contains(&expected_args)
        {
            return Err(
                "ExecStart загруженной службы не соответствует установленному бинарнику".into(),
            );
        }
    }
    Ok(())
}

/// Общий источник фактов для обоих публичных интерфейсов состояния.
pub fn inspect() -> TimerInspection {
    let mut result = TimerInspection {
        state: "unavailable".into(),
        enabled: None,
        active: None,
        service_active: None,
        installed_root: None,
        service_result: None,
        error: None,
    };
    if let Err(e) = require_user_systemd() {
        result.error = Some(e);
        return result;
    }
    result.state = "unknown".into();
    let facts = (|| {
        let timer = loaded_unit(TIMER_NAME)?;
        let absent = timer.get("LoadState").is_some_and(|v| v == "not-found")
            && timer.get("FragmentPath").is_none_or(String::is_empty);
        result.enabled = Some(query_flag("is-enabled", absent)?);
        result.active = Some(query_flag("is-active", absent)?);
        let service = loaded_unit(SERVICE_NAME)?;
        result.service_active = match service.get("ActiveState").map(String::as_str) {
            Some("inactive" | "failed") => Some(false),
            Some("active" | "activating" | "deactivating" | "reloading") => Some(true),
            _ => return Err("неизвестная активность службы".into()),
        };
        let paths = artifact_paths()?;
        verify_unit(TIMER_NAME, &paths[1], &timer, false)?;
        verify_unit(SERVICE_NAME, &paths[0], &service, false)?;
        if (result.enabled == Some(true) || result.active == Some(true))
            && (absent || service.get("LoadState").is_some_and(|v| v != "loaded"))
        {
            return Err("действующий таймер не имеет загруженной управляемой службы".into());
        }
        result.installed_root = installed_root()?;
        if result.installed_root.is_none()
            && (result.enabled == Some(true)
                || result.active == Some(true)
                || result.service_active == Some(true)
                || artifacts_present()?)
        {
            return Err("принадлежность таймера рабочей области не подтверждена".into());
        }
        result.service_result = service.get("Result").filter(|v| !v.is_empty()).cloned();
        Ok::<_, String>(())
    })();
    if let Err(e) = facts {
        result.error = Some(e);
        return result;
    }
    result.state = if result.enabled == Some(true)
        || result.active == Some(true)
        || result.service_active == Some(true)
    {
        "enabled"
    } else {
        "disabled"
    }
    .into();
    if result.enabled == Some(true) && result.active != Some(true) {
        result.error = Some("автозапуск разрешён, но таймер сейчас не активен".into());
    }
    result
}

pub fn install(root: &Path, current_exe: &Path) -> Result<TimerResult, String> {
    let _lock = crate::guard::MaintenanceLock::acquire()
        .map_err(|e| format!("параллельная операция установки или GC: {e}"))?;
    install_with_lock(root, current_exe, &_lock)
}

pub(crate) fn install_with_lock(
    root: &Path,
    current_exe: &Path,
    _lock: &crate::guard::MaintenanceLock,
) -> Result<TimerResult, String> {
    require_user_systemd()?;
    verify_installation_binding()?;
    // Один пользовательский таймер не должен незаметно переключаться между клонами.
    if let Some(existing) = installed_root()? {
        if existing != root {
            return Err(format!(
                "таймер уже принадлежит {}; сначала явно отключите его, затем установите для {}",
                existing.display(),
                root.display()
            ));
        }
    } else if config_home()
        .is_some_and(|home| home.join(format!("systemd/user/{SERVICE_NAME}")).exists())
    {
        return Err("найден существующий service без подтверждённой привязки к checkout; автоматическая переустановка запрещена".into());
    }
    let cargo_bin_dir = resolve_cargo_bin_dir()?;
    let service_unit = render_service_unit(&cargo_bin_dir)?;
    let config_home = config_home().ok_or("не задан HOME или XDG_CONFIG_HOME")?;
    let unit_dir = config_home.join("systemd/user");
    let app_config = config_home.join(format!("anki-decks/{INSTALLED_CONFIG_NAME}"));
    let bin_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("не задан HOME")?
        .join(".local/bin");
    fs::create_dir_all(&unit_dir)
        .map_err(|error| format!("не удалось создать {}: {error}", unit_dir.display()))?;
    ensure_directory(&unit_dir)?;
    private_directory(&config_home.join("anki-decks"))?;
    ensure_directory(&bin_dir)?;

    let service_path = unit_dir.join(SERVICE_NAME);
    let timer_path = unit_dir.join(TIMER_NAME);
    ensure_managed_or_absent(&service_path)?;
    ensure_managed_or_absent(&timer_path)?;
    ensure_installed_config_or_absent(&app_config)?;
    let installed_binary = bin_dir.join("anki-repository-maintenance");
    ensure_binary_managed_or_absent(&installed_binary, &service_path, &app_config)?;
    atomic_write(
        &service_path,
        format!("{MANAGED_MARKER}\n{service_unit}").as_bytes(),
        0o644,
    )?;
    atomic_write(
        &timer_path,
        format!("{MANAGED_MARKER}\n{TIMER_TEMPLATE}").as_bytes(),
        0o644,
    )?;
    let config_text = toml::to_string(&InstalledConfig {
        managed_by: "repository-maintenance",
        schema: 1,
        repository_root: root,
    })
    .map_err(|error| format!("не удалось подготовить конфигурацию установки: {error}"))?;
    atomic_write(&app_config, config_text.as_bytes(), 0o600)?;
    install_binary(current_exe, &installed_binary)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", TIMER_NAME])?;
    let status = systemctl(&["status", TIMER_NAME, "--no-pager"])?;
    let facts = inspect();
    if facts.state != "enabled"
        || facts.enabled != Some(true)
        || facts.active != Some(true)
        || facts.installed_root.as_deref() != Some(root)
    {
        return Err(format!(
            "после установки таймер не подтверждён: {}; {:?}",
            facts.state, facts.error
        ));
    }
    Ok(TimerResult {
        state: "enabled",
        message: "ежедневный пользовательский таймер systemd установлен и включён".into(),
        command_output: Some(status),
    })
}

fn verify_installation_binding() -> Result<(), String> {
    let paths = artifact_paths()?;
    verify_unit(TIMER_NAME, &paths[1], &loaded_unit(TIMER_NAME)?, true)?;
    verify_unit(SERVICE_NAME, &paths[0], &loaded_unit(SERVICE_NAME)?, true)?;
    Ok(())
}

pub fn uninstall() -> Result<TimerResult, String> {
    uninstall_inner(None)
}

/// Удаление с повторной проверкой checkout под той же блокировкой, что и установка.
pub fn uninstall_for(root: &Path) -> Result<TimerResult, String> {
    uninstall_inner(Some(root))
}

fn uninstall_inner(expected_root: Option<&Path>) -> Result<TimerResult, String> {
    let _lock = crate::guard::MaintenanceLock::acquire()
        .map_err(|e| format!("параллельная операция удаления или GC: {e}"))?;
    uninstall_with_lock(expected_root, &_lock)
}

pub(crate) fn uninstall_with_lock(
    expected_root: Option<&Path>,
    _lock: &crate::guard::MaintenanceLock,
) -> Result<TimerResult, String> {
    require_user_systemd()?;
    verify_installation_binding()?;
    installed_root()?;
    if let Some(expected) = expected_root
        && installed_root()?.as_deref() != Some(expected)
    {
        return Err("таймер больше не принадлежит выбранному checkout; удаление запрещено".into());
    }
    let config_home = config_home().ok_or("не задан HOME или XDG_CONFIG_HOME")?;
    let unit_dir = config_home.join("systemd/user");
    let service_path = unit_dir.join(SERVICE_NAME);
    let timer_path = unit_dir.join(TIMER_NAME);
    let app_config = config_home.join(format!("anki-decks/{INSTALLED_CONFIG_NAME}"));
    let mut has_managed_unit = false;
    for path in [&service_path, &timer_path] {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                ensure_managed(path)?;
                has_managed_unit = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(format!("не удалось проверить {}: {error}", path.display()));
            }
        }
    }
    if has_managed_unit {
        systemctl(&["disable", "--now", TIMER_NAME])?;
        systemctl(&["stop", SERVICE_NAME])?;
        if query_flag("is-enabled", false)?
            || query_flag("is-active", false)?
            || !matches!(
                loaded_unit(SERVICE_NAME)?
                    .get("ActiveState")
                    .map(String::as_str),
                Some("inactive" | "failed")
            )
        {
            return Err("таймер или служба остались активны; файлы сохранены".into());
        }
    }
    let mut managed_config = false;
    if let Ok(metadata) = fs::symlink_metadata(&app_config) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "{} не является обычным файлом конфигурации установки",
                app_config.display()
            ));
        }
        let text = fs::read_to_string(&app_config)
            .map_err(|error| format!("не удалось проверить {}: {error}", app_config.display()))?;
        managed_config = text.starts_with("managed_by = \"repository-maintenance\"");
    }
    let installed_binary = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("не задан HOME")?
        .join(".local/bin/anki-repository-maintenance");
    let binary_exists = match fs::symlink_metadata(&installed_binary) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "не удалось проверить {}: {error}",
                installed_binary.display()
            ));
        }
    };
    if binary_exists && (has_managed_unit || managed_config) {
        ensure_binary_managed_or_absent(&installed_binary, &service_path, &app_config)?;
    }
    for path in [&service_path, &timer_path] {
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(format!("не удалось удалить {}: {error}", path.display())),
        }
    }
    if managed_config {
        fs::remove_file(&app_config)
            .map_err(|error| format!("не удалось удалить конфигурацию установки: {error}"))?;
    }
    if binary_exists && (has_managed_unit || managed_config) {
        fs::remove_file(&installed_binary).map_err(|error| {
            format!("не удалось удалить установленный исполняемый файл: {error}")
        })?;
    }
    systemctl(&["daemon-reload"])?;
    Ok(TimerResult {
        state: "disabled",
        message: "ежедневный таймер отключён; его служебные файлы, конфигурация установки и исполняемый файл удалены"
            .into(),
        command_output: None,
    })
}

pub fn status() -> Result<TimerResult, String> {
    let facts = inspect();
    if matches!(facts.state.as_str(), "unknown" | "unavailable") {
        return Err(facts
            .error
            .unwrap_or_else(|| "состояние таймера неизвестно".into()));
    }
    Ok(TimerResult {
        state: if facts.state == "enabled" {
            "enabled"
        } else {
            "disabled"
        },
        message: format!(
            "автозапуск: {}; таймер активен: {}; служба активна: {}",
            flag_text(facts.enabled),
            flag_text(facts.active),
            flag_text(facts.service_active)
        ),
        command_output: facts.error,
    })
}

fn flag_text(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "да",
        Some(false) => "нет",
        None => "неизвестно",
    }
}

fn require_user_systemd() -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map_err(|error| format!("пользовательский менеджер systemd недоступен: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!(
        "пользовательский менеджер systemd недоступен; ручные `scan` и `clean` продолжают работать: {}",
        if message.is_empty() {
            "нет ответа от systemctl"
        } else {
            &message
        }
    ))
}

fn systemctl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| format!("не удалось запустить systemctl --user: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "systemctl --user {} завершился с кодом {:?}: {}",
            args.join(" "),
            output.status.code(),
            stderr
        ));
    }
    Ok(stdout)
}

fn resolve_cargo_bin_dir() -> Result<PathBuf, String> {
    let search_path = std::env::var_os("PATH")
        .ok_or("не задан PATH; установщик не может найти исполняемый Cargo")?;
    for directory in std::env::split_paths(&search_path) {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        let candidate = directory.join("cargo");
        let metadata = match fs::metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            continue;
        }
        let resolved = candidate.canonicalize().map_err(|error| {
            format!(
                "не удалось разрешить найденный Cargo {}: {error}",
                candidate.display()
            )
        })?;
        let resolved_metadata = fs::metadata(&resolved).map_err(|error| {
            format!("не удалось проверить Cargo {}: {error}", resolved.display())
        })?;
        if !resolved_metadata.is_file() || resolved_metadata.mode() & 0o111 == 0 {
            return Err(format!(
                "найденный Cargo {} не является исполняемым обычным файлом",
                candidate.display()
            ));
        }
        let output = Command::new(&candidate)
            .arg("--version")
            .output()
            .map_err(|error| {
                format!(
                    "не удалось запустить найденный Cargo {}: {error}",
                    candidate.display()
                )
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!(
                "найденный Cargo {} недействителен: команда --version завершилась с кодом {:?}{}",
                candidate.display(),
                output.status.code(),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
        return candidate
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .map_err(|error| {
                format!(
                    "не удалось разрешить каталог найденного Cargo {}: {error}",
                    candidate.display()
                )
            });
    }
    Err("не удалось найти исполняемый Cargo в PATH; установите Rust/Cargo и повторите `timer install`".into())
}

fn render_service_unit(cargo_bin_dir: &Path) -> Result<String, String> {
    let cargo_bin_dir = cargo_bin_dir
        .to_str()
        .ok_or("путь к Cargo содержит символы, которые systemd не поддерживает")?;
    if !Path::new(cargo_bin_dir).is_absolute() {
        return Err("путь к каталогу Cargo должен быть абсолютным".into());
    }
    if cargo_bin_dir.contains(':') {
        return Err("путь к каталогу Cargo содержит ':' и не может быть сохранён в PATH".into());
    }
    let path =
        format!("{cargo_bin_dir}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    let escaped_path = escape_systemd_value(&path)?;
    if SERVICE_TEMPLATE.matches(CARGO_PATH_MARKER).count() != 1 {
        return Err(
            "шаблон служебного файла systemd должен содержать ровно одно место для подстановки пути Cargo из `PATH`"
                .into(),
        );
    }
    Ok(SERVICE_TEMPLATE.replace(
        CARGO_PATH_MARKER,
        &format!("Environment=\"PATH={escaped_path}\""),
    ))
}

fn escape_systemd_value(value: &str) -> Result<String, String> {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\0'..='\u{1f}' | '\u{7f}' => {
                return Err(
            "путь к Cargo содержит управляющий символ, недопустимый в служебном файле systemd".into(),
                );
            }
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}

fn ensure_managed_or_absent(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(format!(
            "{} является symlink; файл не изменён",
            path.display()
        ));
    }
    match fs::read_to_string(path) {
        Ok(text) if text.starts_with(MANAGED_MARKER) => Ok(()),
        Ok(_) => Err(format!(
            "{} уже существует и не был создан инструментом `repository-maintenance`; файл не перезаписан",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("не удалось проверить {}: {error}", path.display())),
    }
}

fn ensure_managed(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("не удалось проверить {}: {error}", path.display()))?;
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
    {
        return Err(format!(
            "{} не является безопасным обычным служебным файлом systemd",
            path.display()
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("не удалось прочитать {}: {error}", path.display()))?;
    if text.starts_with(MANAGED_MARKER) {
        Ok(())
    } else {
        Err(format!(
            "владение служебным файлом systemd `repository-maintenance` не подтверждено: {}",
            path.display()
        ))
    }
}

fn private_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!("{} должен быть обычным каталогом", path.display()));
        }
        Ok(metadata) => {
            let uid = fs::metadata("/proc/self")
                .map_err(|error| error.to_string())?
                .uid();
            if metadata.uid() != uid {
                return Err(format!("{} принадлежит другому UID", path.display()));
            }
            if metadata.mode() & 0o077 != 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
                    format!("не удалось закрыть права {}: {error}", path.display())
                })?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|error| format!("не удалось создать {}: {error}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("не удалось закрыть права {}: {error}", path.display()))?;
        }
        Err(error) => return Err(format!("не удалось проверить {}: {error}", path.display())),
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(format!("{} должен быть обычным каталогом", path.display()))
        }
        Ok(metadata) => {
            let uid = fs::metadata("/proc/self")
                .map_err(|error| error.to_string())?
                .uid();
            if metadata.uid() == uid {
                Ok(())
            } else {
                Err(format!("{} принадлежит другому UID", path.display()))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| error.to_string())
        }
        Err(error) => Err(format!("не удалось проверить {}: {error}", path.display())),
    }
}

fn ensure_binary_managed_or_absent(
    binary: &Path,
    service: &Path,
    config: &Path,
) -> Result<(), String> {
    match fs::symlink_metadata(binary) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "не удалось проверить {}: {error}",
            binary.display()
        )),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("{} не является обычным файлом", binary.display()));
            }
            let uid = fs::metadata("/proc/self")
                .map_err(|error| error.to_string())?
                .uid();
            if metadata.uid() != uid || metadata.nlink() != 1 || metadata.mode() & 0o022 != 0 {
                return Err(format!(
                    "{} имеет небезопасного владельца, ссылки или права",
                    binary.display()
                ));
            }
            let service_owned = match fs::symlink_metadata(service) {
                Ok(_) => {
                    ensure_managed(service)?;
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(format!(
                        "не удалось проверить {}: {error}",
                        service.display()
                    ));
                }
            };
            ensure_installed_config_or_absent(config)?;
            if !service_owned && !config.exists() {
                return Err(format!(
                    "{} уже существует, но подтверждённая конфигурация установки не найдена",
                    binary.display()
                ));
            }
            Ok(())
        }
    }
}

fn ensure_installed_config_or_absent(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "{} не является обычным файлом конфигурации установки",
            path.display()
        ));
    }
    match fs::read_to_string(path) {
        Ok(text) if text.starts_with("managed_by = \"repository-maintenance\"") => Ok(()),
        Ok(_) => Err(format!(
            "{} уже существует и не принадлежит установщику; файл не перезаписан",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("не удалось проверить {}: {error}", path.display())),
    }
}

fn install_binary(source: &Path, destination: &Path) -> Result<(), String> {
    let source = source.canonicalize().map_err(|error| {
        format!("не удалось определить путь к текущему исполняемому файлу: {error}")
    })?;
    if source == destination {
        return Ok(());
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = destination.with_extension(format!("install-{}-{suffix}", std::process::id()));
    fs::copy(&source, &temporary).map_err(|error| {
        format!("не удалось скопировать исполняемый файл во временный файл установки: {error}")
    })?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("не удалось установить право на выполнение: {error}"))?;
    fs::rename(&temporary, destination).map_err(|error| {
        format!(
            "не удалось установить исполняемый файл {}: {error}",
            destination.display()
        )
    })
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("install-{}-{suffix}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|error| format!("не удалось создать {}: {error}", temporary.display()))?;
    use std::io::Write;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("не удалось записать {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("не удалось установить {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Output};

    const HELPER_ENV: &str = "ANKI_REPOSITORY_MAINTENANCE_TIMER_TEST_ACTION";

    #[test]
    fn timer_subprocess_helper() {
        let Ok(action) = std::env::var(HELPER_ENV) else {
            return;
        };
        let root =
            PathBuf::from(std::env::var_os("ANKI_REPOSITORY_MAINTENANCE_TEST_ROOT").unwrap());
        let executable = std::env::current_exe().unwrap();
        match action.as_str() {
            "install" => {
                install(&root, &executable).unwrap();
            }
            "status" => {
                status().unwrap();
            }
            "inspect" => {
                let observed = inspect();
                assert_eq!(observed.state, "enabled");
                assert_eq!(observed.installed_root.as_deref(), Some(root.as_path()));
                assert_eq!(observed.active, Some(true));
            }
            "other-checkout" => {
                let another = root.with_file_name("another-checkout");
                fs::create_dir_all(&another).unwrap();
                let err = install(&another, &executable).unwrap_err();
                assert!(err.contains("таймер уже принадлежит"), "{err}");
                assert_eq!(installed_root().unwrap(), Some(root));
            }
            "uninstall" => {
                uninstall().unwrap();
            }
            "service" => {
                run_installed_service().unwrap();
            }
            "service-run" => {
                run_installed_cargo_command().unwrap();
            }
            _ => panic!("неизвестное действие вспомогательного процесса"),
        }
    }

    fn run_installed_service() -> Result<(), String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("переменная HOME не задана")?);
        let config_home = PathBuf::from(
            std::env::var_os("XDG_CONFIG_HOME").ok_or("переменная XDG_CONFIG_HOME не задана")?,
        );
        let installed_binary = home.join(".local/bin/anki-repository-maintenance");
        if !installed_binary.is_file() {
            return Err(format!(
                "установленный исполняемый файл {} отсутствует",
                installed_binary.display()
            ));
        }
        let service_path = config_home.join("systemd/user").join(SERVICE_NAME);
        let service = fs::read_to_string(&service_path)
            .map_err(|error| format!("не удалось прочитать {}: {error}", service_path.display()))?;
        let unit_path = service
            .lines()
            .find_map(parse_service_path)
            .ok_or("в служебном файле systemd не задан `PATH`")?;
        let executable = std::env::current_exe()
            .map_err(|error| format!("не удалось определить вспомогательный процесс: {error}"))?;
        let output = Command::new(&executable)
            .args([
                "--exact",
                "timer::tests::timer_subprocess_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, "service-run")
            .env("PATH", unit_path)
            .output()
            .map_err(|error| {
                format!("не удалось запустить процесс, имитирующий службу: {error}")
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "процесс, имитирующий службу, завершился с кодом {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn run_installed_cargo_command() -> Result<(), String> {
        let root = crate::policy::workspace_root(None, true)?;
        crate::target::discover_workspace(&root).map(|_| ())
    }

    fn parse_service_path(line: &str) -> Option<String> {
        let encoded = line
            .strip_prefix("Environment=\"PATH=")?
            .strip_suffix('"')?;
        let mut decoded = String::with_capacity(encoded.len());
        let mut characters = encoded.chars();
        while let Some(character) = characters.next() {
            if character == '\\' {
                match characters.next()? {
                    '\\' => decoded.push('\\'),
                    '"' => decoded.push('"'),
                    _ => return None,
                }
            } else if character == '%' {
                if characters.next()? != '%' {
                    return None;
                }
                decoded.push('%');
            } else {
                decoded.push(character);
            }
        }
        Some(decoded)
    }

    #[test]
    fn fake_systemd_installer_is_idempotent_and_uninstall_removes_owned_files() {
        let owner = TempWorkspace::create("repository-maintenance-timer-test").unwrap();
        let home = owner.path().join("home");
        let config_home = owner.path().join("config");
        let fake_bin = owner.path().join(r#"fake cargo % " quote \ slash"#);
        let root = owner.path().join("checkout");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let log = owner.path().join("systemctl.log");
        let systemctl = fake_bin.join("systemctl");
        fs::write(
            &systemctl,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
case "$2" in
is-enabled) if [ -f "$HOME/active" ]; then echo enabled; else echo disabled; exit 1; fi;;
is-active) if [ -f "$HOME/active" ]; then echo active; else echo inactive; exit 3; fi;;
enable) : > "$HOME/active";;
disable) /bin/rm -f "$HOME/active";;
show)
name="$3"
fragment="$XDG_CONFIG_HOME/systemd/user/$name"
if [ -f "$fragment" ]; then echo LoadState=loaded; echo "FragmentPath=$fragment";
else echo LoadState=not-found; echo FragmentPath=; fi
echo ActiveState=inactive
echo Result=success
echo DropInPaths=
echo NeedDaemonReload=no
execpath="$HOME/.local/bin/anki-repository-maintenance"
echo "ExecStart={ path=$execpath ; argv[]=$execpath clean --apply --installed-root ; }"
 ;;
esac
exit 0
"#,
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let cargo_log = owner.path().join("cargo.log");
        let cargo = fake_bin.join("cargo");
        fs::write(
            &cargo,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CARGO_LOG\"\ncase \"$1\" in\n  --version) echo 'cargo 1.0.0';;\n  metadata) printf '{\"workspace_root\":\"%s\",\"target_directory\":\"%s/target\"}\\n' \"$ANKI_REPOSITORY_MAINTENANCE_TEST_ROOT\" \"$ANKI_REPOSITORY_MAINTENANCE_TEST_ROOT\";;\n  clean) :;;\n  *) exit 2;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", fake_bin.display());
        let executable = std::env::current_exe().unwrap();
        let helper = HelperHarness {
            executable: &executable,
            root: &root,
            home: &home,
            config_home: &config_home,
            path: &path,
            systemctl_log: &log,
            cargo_log: &cargo_log,
        };

        for action in [
            "install",
            "install",
            "service",
            "status",
            "inspect",
            "other-checkout",
        ] {
            let output = helper.invoke(action);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let unit_dir = config_home.join("systemd/user");
        let installed_service = fs::read_to_string(unit_dir.join(SERVICE_NAME)).unwrap();
        assert!(SERVICE_TEMPLATE.contains("%h/.local/bin/anki-repository-maintenance"));
        assert!(SERVICE_TEMPLATE.contains("clean --apply --installed-root"));
        assert!(!SERVICE_TEMPLATE.contains("--cleanup-legacy"));
        assert!(SERVICE_TEMPLATE.contains(CARGO_PATH_MARKER));
        assert!(!installed_service.contains(CARGO_PATH_MARKER));
        assert!(!installed_service.contains("--cleanup-legacy"));
        assert!(installed_service.contains(r#"fake cargo %% \" quote \\ slash"#));
        assert!(!installed_service.contains("/home/"));
        assert!(!installed_service.contains("/mnt/c/"));
        assert!(installed_service.contains("Environment=\"PATH="));
        let missing_cargo_bin = owner.path().join("systemctl-only");
        fs::create_dir_all(&missing_cargo_bin).unwrap();
        fs::copy(&systemctl, missing_cargo_bin.join("systemctl")).unwrap();
        let missing_cargo_path = missing_cargo_bin.display().to_string();
        let output = helper.invoke_with_path("install", &missing_cargo_path);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("не удалось найти исполняемый Cargo")
        );

        let invalid_cargo_bin = owner.path().join("invalid-cargo");
        fs::create_dir_all(&invalid_cargo_bin).unwrap();
        let invalid_systemctl = invalid_cargo_bin.join("systemctl");
        fs::copy(&systemctl, &invalid_systemctl).unwrap();
        fs::set_permissions(&invalid_systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let invalid_cargo = invalid_cargo_bin.join("cargo");
        fs::write(&invalid_cargo, "#!/bin/sh\necho 'not Cargo' >&2\nexit 17\n").unwrap();
        fs::set_permissions(&invalid_cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let invalid_cargo_path = invalid_cargo_bin.display().to_string();
        let output = helper.invoke_with_path("install", &invalid_cargo_path);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("найденный Cargo"), "{error}");
        assert!(error.contains("--version"), "{error}");

        let output = helper.invoke("uninstall");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!unit_dir.join(SERVICE_NAME).exists());
        assert!(!unit_dir.join(TIMER_NAME).exists());
        assert!(!home.join(".local/bin/anki-repository-maintenance").exists());
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("enable --now").count(), 2);
        assert!(calls.contains("is-enabled"));
        assert!(calls.contains("disable --now"));
        assert!(fs::read_to_string(&cargo_log).unwrap().contains("metadata"));
    }

    struct HelperHarness<'a> {
        executable: &'a Path,
        root: &'a Path,
        home: &'a Path,
        config_home: &'a Path,
        path: &'a str,
        systemctl_log: &'a Path,
        cargo_log: &'a Path,
    }

    impl HelperHarness<'_> {
        fn invoke(&self, action: &str) -> Output {
            self.invoke_with_path(action, self.path)
        }

        fn invoke_with_path(&self, action: &str, path: &str) -> Output {
            Command::new(self.executable)
                .args([
                    "--exact",
                    "timer::tests::timer_subprocess_helper",
                    "--nocapture",
                ])
                .env(HELPER_ENV, action)
                .env("ANKI_REPOSITORY_MAINTENANCE_TEST_ROOT", self.root)
                .env("HOME", self.home)
                .env("XDG_CONFIG_HOME", self.config_home)
                .env(
                    "PATH",
                    if action == "service" {
                        "/usr/bin:/bin"
                    } else {
                        path
                    },
                )
                .env("SYSTEMCTL_LOG", self.systemctl_log)
                .env("CARGO_LOG", self.cargo_log)
                .output()
                .unwrap()
        }
    }
}
