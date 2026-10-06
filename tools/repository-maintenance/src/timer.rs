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
const MANAGED_MARKER: &str = "# Managed by repository-maintenance; do not edit by hand.";

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
        format!("{MANAGED_MARKER}\n{SERVICE_TEMPLATE}").as_bytes(),
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
    .map_err(|error| format!("не удалось подготовить install config: {error}"))?;
    atomic_write(&app_config, config_text.as_bytes(), 0o600)?;
    install_binary(current_exe, &installed_binary)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", TIMER_NAME])?;
    let status = systemctl(&["status", TIMER_NAME, "--no-pager"])?;
    Ok(TimerResult {
        state: "enabled",
        message: "ежедневный user-level systemd timer установлен и включён".into(),
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
                "{} не является обычным install config",
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
            .map_err(|error| format!("не удалось удалить install config: {error}"))?;
    }
    if binary_exists && (has_managed_unit || managed_config) {
        fs::remove_file(&installed_binary)
            .map_err(|error| format!("не удалось удалить установленный binary: {error}"))?;
    }
    systemctl(&["daemon-reload"])?;
    Ok(TimerResult {
        state: "disabled",
        message: "ежедневный timer отключён; его unit-файлы, install config и binary удалены"
            .into(),
        command_output: None,
    })
}

pub fn status() -> Result<TimerResult, String> {
    require_user_systemd()?;
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", TIMER_NAME])
        .output()
        .map_err(|error| format!("не удалось проверить timer: {error}"))?;
    if !enabled.status.success() {
        return Ok(TimerResult {
            state: "disabled",
            message: "timer не включён".into(),
            command_output: Some(String::from_utf8_lossy(&enabled.stdout).trim().to_owned()),
        });
    }
    let output = systemctl(&["status", TIMER_NAME, "--no-pager"])?;
    Ok(TimerResult {
        state: "enabled",
        message: "состояние user-level systemd timer".into(),
        command_output: Some(output),
    })
}

fn require_user_systemd() -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map_err(|error| format!("systemd user manager недоступен: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!(
        "systemd user manager недоступен; ручные scan/clean продолжают работать: {}",
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
            "{} уже существует и не создан repository-maintenance; файл не перезаписан",
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
            "{} не является обычным unit-файлом",
            path.display()
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("не удалось прочитать {}: {error}", path.display()))?;
    if text.starts_with(MANAGED_MARKER) {
        Ok(())
    } else {
        Err(format!(
            "{} не подтверждён как unit repository-maintenance",
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
                    "{} уже существует, но не найдена owned install config",
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
            "{} не является обычным install config",
            path.display()
        ));
    }
    match fs::read_to_string(path) {
        Ok(text) if text.starts_with("managed_by = \"repository-maintenance\"") => Ok(()),
        Ok(_) => Err(format!(
            "{} уже существует и не принадлежит installer; файл не перезаписан",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("не удалось проверить {}: {error}", path.display())),
    }
}

fn install_binary(source: &Path, destination: &Path) -> Result<(), String> {
    let source = source
        .canonicalize()
        .map_err(|error| format!("не удалось разрешить текущий binary: {error}"))?;
    if source == destination {
        return Ok(());
    }
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = destination.with_extension(format!("install-{}-{suffix}", std::process::id()));
    fs::copy(&source, &temporary)
        .map_err(|error| format!("не удалось скопировать binary в staging: {error}"))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("не удалось выставить executable bit: {error}"))?;
    fs::rename(&temporary, destination).map_err(|error| {
        format!(
            "не удалось установить binary {}: {error}",
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
            "install" => install(&root, &executable).unwrap(),
            "status" => status().unwrap(),
            "uninstall" => uninstall().unwrap(),
            _ => panic!("unknown helper action"),
        };
    }

    #[test]
    fn fake_systemd_installer_is_idempotent_and_uninstall_removes_owned_files() {
        let owner = TempWorkspace::create("repository-maintenance-timer-test").unwrap();
        let home = owner.path().join("home");
        let config_home = owner.path().join("config");
        let fake_bin = owner.path().join("fake-bin");
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
        let path = format!("{}:/usr/bin:/bin", fake_bin.display());
        let executable = std::env::current_exe().unwrap();

        for action in ["install", "install", "status", "uninstall"] {
            let output =
                invoke_helper(&executable, action, &root, &home, &config_home, &path, &log);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let unit_dir = config_home.join("systemd/user");
        assert!(!unit_dir.join(SERVICE_NAME).exists());
        assert!(!unit_dir.join(TIMER_NAME).exists());
        assert!(!home.join(".local/bin/anki-repository-maintenance").exists());
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("enable --now").count(), 2);
        assert!(calls.contains("is-enabled"));
        assert!(calls.contains("disable --now"));
        assert!(SERVICE_TEMPLATE.contains("%h/.local/bin/anki-repository-maintenance"));
        assert!(!SERVICE_TEMPLATE.contains("/home/"));
        assert!(!SERVICE_TEMPLATE.contains("/mnt/c/"));
    }

    fn invoke_helper(
        executable: &Path,
        action: &str,
        root: &Path,
        home: &Path,
        config_home: &Path,
        path: &str,
        log: &Path,
    ) -> Output {
        Command::new(executable)
            .args([
                "--exact",
                "timer::tests::timer_subprocess_helper",
                "--nocapture",
            ])
            .env(HELPER_ENV, action)
            .env("ANKI_REPOSITORY_MAINTENANCE_TEST_ROOT", root)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", config_home)
            .env("PATH", path)
            .env("SYSTEMCTL_LOG", log)
            .output()
            .unwrap()
    }
}
