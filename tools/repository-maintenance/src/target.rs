use std::fs;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::{Mode, OFlags, open, openat};
use serde::Deserialize;

use crate::inventory::{Measurement, measure_path};
use crate::process::{ProcessCheck, target_process_check};

const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

#[derive(Debug, Clone)]
pub struct CargoWorkspace {
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub target_dir: PathBuf,
    pub build_dir: Option<PathBuf>,
    pub measured_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdDecision {
    Keep,
    Warn,
    Clean,
}

pub fn threshold_decision(
    allocated_bytes: u64,
    warning_bytes: u64,
    hard_limit_bytes: u64,
) -> ThresholdDecision {
    if allocated_bytes >= hard_limit_bytes {
        ThresholdDecision::Clean
    } else if allocated_bytes >= warning_bytes {
        ThresholdDecision::Warn
    } else {
        ThresholdDecision::Keep
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CargoTargetMeasurement {
    pub workspace_root: PathBuf,
    pub target_dir: PathBuf,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub dirs: Vec<Measurement>,
    pub decision: ThresholdDecision,
    pub safety: ProcessCheck,
    pub safe_to_clean: bool,
    pub ownership_safe: bool,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
}

pub fn discover_workspace(root: &Path) -> Result<CargoWorkspace, String> {
    let manifest = root.join("Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--manifest-path",
        ])
        .arg(&manifest)
        .current_dir(root)
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .map_err(|error| format!("не удалось запустить cargo metadata: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo metadata завершился с ошибкой: {}",
            bounded(&stderr)
        ));
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata вернул некорректный JSON: {error}"))?;
    let metadata_root = metadata
        .workspace_root
        .canonicalize()
        .map_err(|error| format!("не удалось определить корень рабочей области Cargo: {error}"))?;
    if metadata_root != root {
        return Err(format!(
            "указанный корень {} не совпадает с корнем рабочей области Cargo {}",
            root.display(),
            metadata_root.display()
        ));
    }
    let target_dir = absolute_from(root, &metadata.target_directory);
    let build_dir = configured_build_dir(root)?;
    let mut measured_dirs = vec![target_dir.clone()];
    if let Some(build_dir) = &build_dir
        && !measured_dirs.contains(build_dir)
    {
        measured_dirs.push(build_dir.clone());
    }
    measured_dirs = reduce_overlapping_dirs(measured_dirs);
    Ok(CargoWorkspace {
        root: root.to_path_buf(),
        manifest,
        target_dir,
        build_dir,
        measured_dirs,
    })
}

fn configured_build_dir(root: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(value) = std::env::var_os("CARGO_BUILD_BUILD_DIR") {
        let value = PathBuf::from(value);
        return Ok(Some(absolute_from(root, &value)));
    }
    let Some(config_home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    else {
        return Ok(None);
    };
    let mut configs = Vec::new();
    let mut cursor = root.to_path_buf();
    loop {
        for filename in ["config", "config.toml"] {
            let config = cursor.join(".cargo").join(filename);
            if config.is_file() {
                configs.push(config);
                break;
            }
        }
        if !cursor.pop() {
            break;
        }
    }
    for filename in ["config", "config.toml"] {
        let home_config = config_home.join(filename);
        if home_config.is_file() {
            configs.push(home_config);
            break;
        }
    }
    for config in configs {
        let text = fs::read_to_string(&config)
            .map_err(|error| format!("не удалось прочитать {}: {error}", config.display()))?;
        let value: toml::Value = toml::from_str(&text)
            .map_err(|error| format!("не удалось разобрать {}: {error}", config.display()))?;
        let Some(build_dir) = value.get("build").and_then(|build| build.get("build-dir")) else {
            continue;
        };
        let Some(build_dir) = build_dir.as_str() else {
            return Err(format!(
                "параметр Cargo `build.build-dir` в {} должен быть строкой",
                config.display()
            ));
        };
        if build_dir.contains('{') || build_dir.contains('}') {
            return Err(format!(
                "параметр Cargo `build.build-dir` в {} использует шаблон пути; очистка отложена, пока путь нельзя надёжно определить",
                config.display()
            ));
        }
        let config_dir = config.parent().and_then(Path::parent).unwrap_or(root);
        let path = PathBuf::from(build_dir);
        let absolute = if path.is_absolute() {
            path
        } else {
            config_dir.join(path)
        };
        return Ok(Some(absolute));
    }
    Ok(None)
}

fn absolute_from(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn reduce_overlapping_dirs(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut reduced = Vec::new();
    for path in paths {
        if reduced
            .iter()
            .any(|parent: &PathBuf| path.starts_with(parent))
        {
            continue;
        }
        reduced.push(path);
    }
    reduced
}

pub fn inspect_target(
    workspace: &CargoWorkspace,
    warning_bytes: u64,
    hard_limit_bytes: u64,
) -> CargoTargetMeasurement {
    let mut dirs = Vec::new();
    let mut allocated = 0_u64;
    let mut apparent = 0_u64;
    let mut errors = Vec::new();
    let mut mount_boundaries = 0_usize;
    for path in &workspace.measured_dirs {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                errors.push(format!(
                    "{} — `target` является символической ссылкой",
                    path.display()
                ));
                continue;
            }
            Ok(metadata) if !metadata.is_dir() => {
                errors.push(format!(
                    "{} — `target` не является каталогом",
                    path.display()
                ));
                continue;
            }
            Ok(_) => (),
        }
        match measure_path(path) {
            Ok(measurement) => {
                allocated = allocated.saturating_add(measurement.allocated_bytes);
                apparent = apparent.saturating_add(measurement.apparent_bytes);
                mount_boundaries += measurement.mount_boundaries;
                errors.extend(
                    measurement
                        .errors
                        .iter()
                        .map(|error| format!("{}: {error}", path.display())),
                );
                dirs.push(measurement);
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    let decision = threshold_decision(allocated, warning_bytes, hard_limit_bytes);
    let safety = if decision == ThresholdDecision::Clean {
        workspace
            .measured_dirs
            .iter()
            .map(|path| target_process_check(path, &workspace.root))
            .find(|check| !matches!(check, ProcessCheck::Clear))
            .unwrap_or(ProcessCheck::Clear)
    } else {
        ProcessCheck::Clear
    };
    let ownership_safe = workspace
        .measured_dirs
        .iter()
        .all(|path| path_is_within_workspace(&workspace.root, path));
    let safe_to_clean = decision == ThresholdDecision::Clean
        && errors.is_empty()
        && mount_boundaries == 0
        && ownership_safe
        && matches!(safety, ProcessCheck::Clear)
        && validate_cargo_target(&workspace.target_dir).is_ok();
    let reason = if decision == ThresholdDecision::Keep {
        "размер ниже порога предупреждения".into()
    } else if decision == ThresholdDecision::Warn {
        "размер выше порога предупреждения; очистка не запускается".into()
    } else if !errors.is_empty() {
        format!(
            "нельзя безопасно измерить каталог `target`: {}",
            errors.join("; ")
        )
    } else if mount_boundaries != 0 {
        "обнаружена вложенная точка монтирования; очистка всего каталога Cargo `target` отложена"
            .into()
    } else if !ownership_safe {
        "каталог `target` или `build-dir` находится вне workspace root (корня рабочей области) и не имеет маркера владения проекта; очистка отложена".into()
    } else if !matches!(safety, ProcessCheck::Clear) {
        match &safety {
            ProcessCheck::Busy {
                pid,
                process,
                reference,
            } => {
                format!("активен {process} PID {pid}: {reference}")
            }
            ProcessCheck::Unknown { reason } => {
                format!("нельзя доказать отсутствие активной сборки: {reason}")
            }
            ProcessCheck::Clear => unreachable!(),
        }
    } else if let Err(error) = validate_cargo_target(&workspace.target_dir) {
        format!("каталог `target` не подтверждён маркером Cargo `CACHEDIR.TAG`: {error}")
    } else {
        "каталог `target` превысил жёсткий предел и подтверждён Cargo".into()
    };
    CargoTargetMeasurement {
        workspace_root: workspace.root.clone(),
        target_dir: workspace.target_dir.clone(),
        allocated_bytes: allocated,
        apparent_bytes: apparent,
        dirs,
        decision,
        safety,
        safe_to_clean,
        ownership_safe,
        reason,
    }
}

fn path_is_within_workspace(workspace_root: &Path, path: &Path) -> bool {
    let Ok(workspace_root) = workspace_root.canonicalize() else {
        return false;
    };
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let mut cursor = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(_) => {
                let Ok(canonical) = cursor.canonicalize() else {
                    return false;
                };
                return canonical.starts_with(&workspace_root);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !cursor.pop() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

pub fn validate_cargo_target(path: &Path) -> io::Result<()> {
    let directory = open_directory_without_symlinks(path)?;
    let metadata = directory.metadata()?;
    let uid = fs::metadata("/proc/self")?.uid();
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "каталог Cargo `target` принадлежит другому UID",
        ));
    }
    let marker_fd = openat(
        &directory,
        "CACHEDIR.TAG",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut marker = fs::File::from(marker_fd);
    let marker_meta = marker.metadata()?;
    if !marker_meta.is_file() || marker_meta.uid() != uid || marker_meta.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "CACHEDIR.TAG не является обычным файлом текущего UID",
        ));
    }
    let mut signature = vec![0; CACHEDIR_SIGNATURE.len()];
    marker.read_exact(&mut signature)?;
    if signature != CACHEDIR_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "неверная сигнатура CACHEDIR.TAG",
        ));
    }
    Ok(())
}

fn open_directory_without_symlinks(path: &Path) -> io::Result<fs::File> {
    let mut directory = fs::File::from(open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => (),
            std::path::Component::Normal(name) => {
                let next = openat(
                    directory.as_fd(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                directory = fs::File::from(next);
            }
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "путь к каталогу Cargo `target` содержит запрещённый компонент",
                ));
            }
        }
    }
    Ok(directory)
}

pub fn cargo_clean(workspace: &CargoWorkspace) -> Result<(String, String), String> {
    let mut command = Command::new("cargo");
    command
        .arg("clean")
        .arg("--manifest-path")
        .arg(&workspace.manifest)
        .arg("--target-dir")
        .arg(&workspace.target_dir)
        .arg("--locked")
        .arg("--offline")
        .env("CARGO_NET_OFFLINE", "true")
        .env(
            "CARGO_BUILD_BUILD_DIR",
            workspace
                .build_dir
                .as_deref()
                .unwrap_or(&workspace.target_dir),
        )
        .env("RUSTUP_AUTO_INSTALL", "0")
        .current_dir(&workspace.root);
    let output = command
        .output()
        .map_err(|error| format!("не удалось запустить cargo clean: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "cargo clean завершился с кодом {:?}: {}",
            output.status.code(),
            bounded(&stderr)
        ));
    }
    Ok((stdout, stderr))
}

/// Очищает только явно указанный управляемый внешний каталог сборки. Для него
/// `build-dir` временно совмещается с `target-dir`, чтобы Cargo не затронул
/// второй путь из конфигурации рабочей области.
pub fn cargo_clean_external(workspace: &CargoWorkspace) -> Result<(String, String), String> {
    let mut command = Command::new("cargo");
    command
        .arg("clean")
        .arg("--manifest-path")
        .arg(&workspace.manifest)
        .arg("--target-dir")
        .arg(&workspace.target_dir)
        .arg("--locked")
        .arg("--offline")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_BUILD_BUILD_DIR", &workspace.target_dir)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .current_dir(&workspace.root);
    let output = command
        .output()
        .map_err(|error| format!("не удалось запустить cargo clean: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "cargo clean завершился с кодом {:?}: {}",
            output.status.code(),
            bounded(&stderr)
        ));
    }
    Ok((stdout, stderr))
}

fn bounded(text: &str) -> String {
    const LIMIT: usize = 2000;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let boundary = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= LIMIT)
        .last()
        .unwrap_or(0);
    format!("{}…", &text[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;

    #[test]
    fn threshold_decision_uses_production_policy_at_boundaries() {
        assert_eq!(threshold_decision(99, 100, 200), ThresholdDecision::Keep);
        assert_eq!(threshold_decision(100, 100, 200), ThresholdDecision::Warn);
        assert_eq!(threshold_decision(150, 100, 200), ThresholdDecision::Warn);
        assert_eq!(threshold_decision(200, 100, 200), ThresholdDecision::Clean);
        assert_eq!(threshold_decision(201, 100, 200), ThresholdDecision::Clean);
    }

    #[test]
    fn cargo_target_outside_checkout_is_measured_but_never_auto_cleaned() {
        let owner = TempWorkspace::create("repository-maintenance-unowned-target-test").unwrap();
        let root = owner.path().join("checkout");
        let target = owner.path().join("shared-target");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(
            target.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        fs::write(target.join("artifact"), [0x71_u8; 4096]).unwrap();
        let workspace = CargoWorkspace {
            root,
            manifest: owner.path().join("checkout/Cargo.toml"),
            target_dir: target.clone(),
            build_dir: None,
            measured_dirs: vec![target],
        };
        let measurement = inspect_target(&workspace, 1, 2);
        assert_eq!(measurement.decision, ThresholdDecision::Clean);
        assert!(!measurement.ownership_safe);
        assert!(!measurement.safe_to_clean);
        assert!(measurement.reason.contains("workspace root"));
    }

    #[test]
    fn cargo_clean_apply_removes_only_explicit_synthetic_target() {
        let owner = TempWorkspace::create("repository-maintenance-cargo-clean-test")
            .expect("временная рабочая область проекта создаётся");
        let root = owner.path().join("fixture");
        let target = root.join("target");
        let build_dir = root.join("build");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"../target\"\nbuild-dir = \"../build\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"gc_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        fs::create_dir_all(build_dir.join("debug/incremental/synthetic")).unwrap();
        fs::write(
            build_dir.join("debug/incremental/synthetic/artifact"),
            [3_u8; 4096],
        )
        .unwrap();
        fs::write(root.join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(
            target.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        fs::write(target.join("synthetic-artifact"), [7_u8; 4096]).unwrap();
        let workspace = CargoWorkspace {
            root: root.clone(),
            manifest: root.join("Cargo.toml"),
            target_dir: target.clone(),
            build_dir: Some(build_dir.clone()),
            measured_dirs: vec![target.clone(), build_dir.clone()],
        };
        let before = measure_path(&target).unwrap().allocated_bytes;
        cargo_clean(&workspace).unwrap();
        assert!(!target.exists());
        assert!(
            !build_dir
                .join("debug/incremental/synthetic/artifact")
                .exists()
        );
        assert!(before > 0);
    }
}
