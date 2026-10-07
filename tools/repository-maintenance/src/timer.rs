use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::policy::config_home;

const SERVICE_NAME: &str = "anki-repository-maintenance.service";
const TIMER_NAME: &str = "anki-repository-maintenance.timer";
const INSTALLED_CONFIG_NAME: &str = "repository-maintenance-install.toml";
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

pub fn install(root: &Path, current_exe: &Path) -> Result<TimerResult, String> {
    require_user_systemd()?;
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
    Ok(TimerResult {
        state: "enabled",
        message: "ежедневный пользовательский таймер systemd установлен и включён".into(),
        command_output: Some(status),
    })
}

pub fn uninstall() -> Result<TimerResult, String> {
    require_user_systemd()?;
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
        let _ = systemctl(&["disable", "--now", TIMER_NAME]);
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
    require_user_systemd()?;
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", TIMER_NAME])
        .output()
        .map_err(|error| format!("не удалось проверить таймер: {error}"))?;
    if !enabled.status.success() {
        return Ok(TimerResult {
            state: "disabled",
            message: "таймер не включён".into(),
            command_output: Some(String::from_utf8_lossy(&enabled.stdout).trim().to_owned()),
        });
    }
    let output = systemctl(&["status", TIMER_NAME, "--no-pager"])?;
    Ok(TimerResult {
        state: "enabled",
        message: "состояние пользовательского таймера systemd".into(),
        command_output: Some(output),
    })
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
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} не является обычным служебным файлом systemd",
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
            if metadata.uid() != uid {
                return Err(format!("{} принадлежит другому UID", binary.display()));
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
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SYSTEMCTL_LOG\"\ncase \" $* \" in *' is-enabled '*) echo enabled;; *' status '*) echo active;; esac\nexit 0\n",
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

        for action in ["install", "install", "service", "status"] {
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
        assert!(SERVICE_TEMPLATE.contains(CARGO_PATH_MARKER));
        assert!(!installed_service.contains(CARGO_PATH_MARKER));
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
