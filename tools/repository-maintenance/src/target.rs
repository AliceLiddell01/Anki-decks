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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetBlocker {
    Busy,
    Unsafe(String),
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
    #[serde(skip)]
    pub blocker: Option<TargetBlocker>,
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

/// Отдельный `build-dir`, который удаляется собственным вызовом `cargo clean`.
fn separate_build_dir(workspace: &CargoWorkspace) -> Option<&Path> {
    let build_dir = workspace.build_dir.as_deref()?;
    (build_dir != workspace.target_dir.as_path()).then_some(build_dir)
}

/// Каталоги, которые инструмент удаляет отдельными вызовами `cargo clean`.
/// Существующий отдельный `build-dir` попадает в список наравне с `target`:
/// право удалить его содержимое подтверждается тем же маркером Cargo.
fn cleanup_dirs(workspace: &CargoWorkspace) -> Vec<&Path> {
    let mut dirs = Vec::new();
    for path in [
        Some(workspace.target_dir.as_path()),
        separate_build_dir(workspace),
    ]
    .into_iter()
    .flatten()
    {
        if !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            dirs.push(path);
        }
    }
    dirs
}

/// Наличие маркера Cargo не разрешает удалять исходники или пересекающиеся деревья.
fn validate_cleanup_layout(workspace: &CargoWorkspace) -> Result<(), String> {
    let root = workspace
        .root
        .canonicalize()
        .map_err(|error| format!("не удалось определить корень рабочей области: {error}"))?;
    let mut previous_dirs: Vec<PathBuf> = Vec::new();
    for path in cleanup_dirs(workspace) {
        let canonical = path.canonicalize().map_err(|error| {
            format!("не удалось определить каталог {}: {error}", path.display())
        })?;
        if canonical == root || !canonical.starts_with(&root) {
            return Err(format!(
                "каталог {} пересекается с корнем рабочей области или находится вне корня рабочей области; очистка запрещена",
                path.display()
            ));
        }
        // Эти пути — существующие владельцы исходников и данных репозитория,
        // а не имена конкретных каталогов сборки.
        let protected = [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "AGENTS.md",
            ".git",
            ".github",
            ".agents",
            ".codex",
            "decks",
            "tools",
        ];
        if protected
            .iter()
            .map(|name| root.join(name))
            .any(|item| item.exists() && item.starts_with(&canonical))
            || canonical.join("Cargo.toml").exists()
            || canonical.join("deck.json").exists()
            || canonical.join(".git").exists()
        {
            return Err(format!(
                "каталог {} содержит защищённые данные проекта; очистка запрещена",
                path.display()
            ));
        }
        if previous_dirs
            .iter()
            .any(|previous| canonical.starts_with(previous) || previous.starts_with(&canonical))
        {
            return Err(format!(
                "каталоги сборки пересекаются у {}; очистка запрещена",
                path.display()
            ));
        }
        previous_dirs.push(canonical);
    }
    Ok(())
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
    // Владение подтверждается для всех учитываемых каталогов сборки, а маркер
    // Cargo — для каждого каталога, который инструмент действительно очищает:
    // отдельный `build-dir` удаляется не меньше, чем `target`.
    let cleanup_dirs = cleanup_dirs(workspace);
    let layout_error = if decision == ThresholdDecision::Clean {
        validate_cleanup_layout(workspace).err()
    } else {
        None
    };
    let ownership_safe = layout_error.is_none()
        && workspace
            .measured_dirs
            .iter()
            .all(|path| path_is_within_workspace(&workspace.root, path));
    let marker_error = if decision == ThresholdDecision::Clean {
        cleanup_dirs.iter().find_map(|path| {
            validate_cargo_target(path)
                .err()
                .map(|error| (*path, error))
        })
    } else {
        None
    };
    let safe_to_clean = decision == ThresholdDecision::Clean
        && errors.is_empty()
        && mount_boundaries == 0
        && ownership_safe
        && matches!(safety, ProcessCheck::Clear)
        && marker_error.is_none();
    let reason = if !errors.is_empty() {
        format!(
            "нельзя безопасно измерить каталог `target`: {}",
            errors.join("; ")
        )
    } else if mount_boundaries != 0 {
        "обнаружена вложенная точка монтирования; очистка всего каталога Cargo `target` отложена"
            .into()
    } else if decision == ThresholdDecision::Keep {
        "размер ниже порога предупреждения".into()
    } else if decision == ThresholdDecision::Warn {
        "размер выше порога предупреждения; очистка не запускается".into()
    } else if let Some(error) = &layout_error {
        error.clone()
    } else if !ownership_safe {
        "каталог `target` или `build-dir` находится вне корня рабочей области и не имеет маркера владения проекта; очистка отложена".into()
    } else if let Some((path, error)) = &marker_error {
        format!(
            "каталог `{}` не подтверждён маркером Cargo `CACHEDIR.TAG`: {error}",
            path.display()
        )
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
    } else {
        "каталог `target` превысил жёсткий предел и подтверждён Cargo".into()
    };
    let blocker = if !errors.is_empty() || mount_boundaries != 0 {
        Some(TargetBlocker::Unsafe(reason.clone()))
    } else if decision != ThresholdDecision::Clean || safe_to_clean {
        None
    } else if errors.is_empty()
        && mount_boundaries == 0
        && ownership_safe
        && marker_error.is_none()
        && matches!(safety, ProcessCheck::Busy { .. })
    {
        Some(TargetBlocker::Busy)
    } else {
        Some(TargetBlocker::Unsafe(reason.clone()))
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
        blocker,
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
    // Повторяем проверку даже если функция вызвана без построения плана.
    validate_cleanup_layout(workspace)?;
    let candidates = cleanup_dirs(workspace);
    for path in &candidates {
        validate_cargo_target(path).map_err(|error| {
            format!(
                "каталог {} не подтверждён маркером Cargo `CACHEDIR.TAG`: {error}",
                path.display()
            )
        })?;
    }

    // Отсутствующий target не удаляем: отдельный build-dir можно очистить
    // самостоятельно, без создания или затрагивания основного каталога.
    let (stdout, stderr) = if candidates.contains(&workspace.target_dir.as_path()) {
        let output = Command::new("cargo")
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
            .current_dir(&workspace.root)
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
        (stdout, stderr)
    } else {
        (String::new(), String::new())
    };
    // На закреплённой Cargo 1.88 отдельный build-dir требует своего вызова.
    if let Some(build_dir) = separate_build_dir(workspace)
        && candidates.contains(&build_dir)
        && build_dir.exists()
    {
        let mut separate_build = workspace.clone();
        separate_build.target_dir = build_dir.to_path_buf();
        cargo_clean_external(&separate_build)
            .map_err(|error| format!("не удалось очистить отдельный build-dir: {error}"))?;
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
        assert!(measurement.reason.contains("корня рабочей области"));
    }

    #[test]
    fn incomplete_target_measurement_is_unsafe_below_threshold() {
        let owner = TempWorkspace::create("repository-maintenance-incomplete-target-test").unwrap();
        let root = owner.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target-file");
        fs::write(&target, b"not a directory").unwrap();
        let workspace = CargoWorkspace {
            root,
            manifest: owner.path().join("checkout/Cargo.toml"),
            target_dir: target.clone(),
            build_dir: None,
            measured_dirs: vec![target],
        };

        let measurement = inspect_target(&workspace, 1, 2);

        assert_eq!(measurement.decision, ThresholdDecision::Keep);
        assert!(!measurement.safe_to_clean);
        assert!(matches!(
            measurement.blocker,
            Some(TargetBlocker::Unsafe(_))
        ));
        assert!(measurement.reason.contains("нельзя безопасно измерить"));
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
        fs::write(
            build_dir.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        let untouched = root.join("unrelated-file");
        fs::write(&untouched, "должен остаться").unwrap();
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
        assert_eq!(fs::read_to_string(&untouched).unwrap(), "должен остаться");
        assert!(before > 0);
    }

    #[test]
    fn cargo_clean_refuses_unmarked_separate_build_dir() {
        let owner = TempWorkspace::create("repository-maintenance-unmarked-build-dir-test")
            .expect("временная рабочая область проекта создаётся");
        let root = owner.path().join("fixture");
        let target = root.join("target");
        let build_dir = root.join("build");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
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
        fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        fs::write(
            target.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        let artifact = build_dir.join("debug/incremental/synthetic/artifact");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, [3_u8; 4096]).unwrap();
        let workspace = CargoWorkspace {
            root: root.clone(),
            manifest: root.join("Cargo.toml"),
            target_dir: target.clone(),
            build_dir: Some(build_dir.clone()),
            measured_dirs: vec![target.clone(), build_dir.clone()],
        };
        fs::write(root.join("Cargo.lock"), "version = 3\n").unwrap();

        // Отдельный каталог подтверждён маркером Cargo: очистка выполняется.
        fs::write(
            build_dir.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        cargo_clean(&workspace).unwrap();
        assert!(!build_dir.exists());

        // Без маркера каталог не подтверждён: содержимое остаётся нетронутым.
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, [3_u8; 4096]).unwrap();
        let error = cargo_clean(&workspace).unwrap_err();
        assert!(error.contains("не подтверждён маркером"), "{error}");
        assert!(artifact.exists(), "неподтверждённый каталог не очищается");
    }
    #[test]
    fn cargo_clean_refuses_workspace_root_even_with_marker() {
        let owner = TempWorkspace::create("repository-maintenance-root-guard-test").unwrap();
        let root = owner.path().join("checkout");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(root.join("source.txt"), "сохранить").unwrap();
        for path in [&root, &target] {
            fs::write(
                path.join("CACHEDIR.TAG"),
                [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
            )
            .unwrap();
        }
        fs::write(target.join("artifact"), [8_u8; 4096]).unwrap();
        let workspace = CargoWorkspace {
            root: root.clone(),
            manifest: root.join("Cargo.toml"),
            target_dir: target.clone(),
            build_dir: Some(root.clone()),
            measured_dirs: vec![root.clone()],
        };
        let report = inspect_target(&workspace, 1, 2);
        assert!(!report.safe_to_clean);
        assert!(
            report.reason.contains("корнем рабочей области"),
            "{}",
            report.reason
        );
        assert!(
            cargo_clean(&workspace)
                .unwrap_err()
                .contains("корнем рабочей области")
        );
        assert_eq!(
            fs::read_to_string(root.join("source.txt")).unwrap(),
            "сохранить"
        );
        assert!(target.join("artifact").exists());
    }

    #[test]
    fn cargo_clean_refuses_overlapping_directories() {
        let owner = TempWorkspace::create("repository-maintenance-overlap-guard-test").unwrap();
        let root = owner.path().join("checkout");
        let target = root.join("target");
        let build_dir = target.join("build");
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(root.join("source.txt"), "сохранить").unwrap();
        for path in [&target, &build_dir] {
            fs::write(
                path.join("CACHEDIR.TAG"),
                [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
            )
            .unwrap();
        }
        let workspace = CargoWorkspace {
            root: root.clone(),
            manifest: root.join("Cargo.toml"),
            target_dir: target.clone(),
            build_dir: Some(build_dir),
            measured_dirs: vec![target],
        };
        assert!(!inspect_target(&workspace, 1, 2).safe_to_clean);
        assert!(
            cargo_clean(&workspace)
                .unwrap_err()
                .contains("пересекаются")
        );
        assert_eq!(
            fs::read_to_string(root.join("source.txt")).unwrap(),
            "сохранить"
        );
    }

    #[test]
    fn cargo_clean_cleans_build_without_primary_target() {
        let owner = TempWorkspace::create("repository-maintenance-build-only-test").unwrap();
        let root = owner.path().join("fixture");
        let target = root.join("target");
        let build_dir = root.join("build");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(root.join("Cargo.toml"),
            "[package]\nname = \"gc_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        fs::write(root.join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(
            build_dir.join("CACHEDIR.TAG"),
            [CACHEDIR_SIGNATURE, b"\nGenerated by Cargo\n"].concat(),
        )
        .unwrap();
        fs::write(build_dir.join("artifact"), [9_u8; 4096]).unwrap();
        let workspace = CargoWorkspace {
            root: root.clone(),
            manifest: root.join("Cargo.toml"),
            target_dir: target.clone(),
            build_dir: Some(build_dir.clone()),
            measured_dirs: vec![target.clone(), build_dir.clone()],
        };
        let report = inspect_target(&workspace, 1, 2);
        assert!(report.safe_to_clean, "{}", report.reason);
        cargo_clean(&workspace).unwrap();
        assert!(!build_dir.exists());
        assert!(!target.exists());
        assert!(root.join("src/lib.rs").exists());
    }
}
