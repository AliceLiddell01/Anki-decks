use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
use serde::{Deserialize, Serialize};

use crate::guard::MaintenanceLock;
use crate::inventory::{Measurement, measure_path};
use crate::process::{ProcessCheck, target_process_check};
use crate::target::{CargoWorkspace, cargo_clean_external, validate_cargo_target};

const MARKER: &str = ".anki-decks-build-cache.json";
const MARKER_LIMIT: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ExternalCacheInventory {
    #[serde(serialize_with = "crate::serialize_report_path")]
    pub root: PathBuf,
    pub allocated_bytes: u64,
    pub managed_entries: usize,
    pub unknown_entries: usize,
    pub candidates: Vec<ExternalCacheCandidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalCacheCandidate {
    pub id: String,
    #[serde(serialize_with = "crate::serialize_report_path")]
    pub path: PathBuf,
    #[serde(serialize_with = "crate::serialize_optional_report_path")]
    pub target_dir: Option<PathBuf>,
    pub allocated_bytes: u64,
    pub bytes_after: Option<u64>,
    pub bytes_freed: u64,
    pub last_used_unix_seconds: Option<u64>,
    pub age_seconds: Option<u64>,
    pub ownership: &'static str,
    pub live: Option<bool>,
    pub action: &'static str,
    pub reason: String,
    pub error: Option<String>,
    #[serde(skip)]
    pub(crate) measurement: Option<Measurement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildCacheMarker {
    pub schema: u32,
    pub repository_root: PathBuf,
    pub cache_id: String,
    pub target_dir: PathBuf,
    pub created_unix_seconds: u64,
    pub last_used_unix_seconds: u64,
}

/// Удерживает общую блокировку обслуживания от проверки происхождения до завершения
/// дочерней команды. Вызывающий код должен дождаться команды перед `finish`; при
/// раннем возврате `Drop` обновит маркер.
pub struct CacheRunGuard {
    _lock: MaintenanceLock,
    directory: File,
    cache_dir: PathBuf,
    target_dir: PathBuf,
    marker: BuildCacheMarker,
    uid: u32,
    active: bool,
}

impl CacheRunGuard {
    /// Блокировка передаётся уже захваченной: повторный `flock` и ожидание сборщика
    /// мусора здесь не нужны.
    pub fn begin(
        repository_root: &Path,
        cache_root: &Path,
        id: &str,
        lock: MaintenanceLock,
    ) -> Result<Self, String> {
        let (cache_dir, target_dir) = resolve_build_cache(repository_root, cache_root, id)?;
        let uid = fs::metadata("/proc/self")
            .map_err(|error| error.to_string())?
            .uid();
        let directory =
            open_private_directory(&cache_dir, uid).map_err(|error| error.to_string())?;
        let marker = read_marker_at(&directory, uid).map_err(|error| error.to_string())?;
        if marker.schema != 1
            || marker.repository_root
                != repository_root
                    .canonicalize()
                    .map_err(|error| error.to_string())?
            || marker.cache_id != id
            || !valid_relative_target(&marker.target_dir)
            || cache_dir.join(&marker.target_dir) != target_dir
        {
            return Err("маркер владения кэшем изменился перед запуском команды".into());
        }
        validate_managed_target_path(&cache_dir, &marker.target_dir, uid)?;
        let mut guard = Self {
            _lock: lock,
            directory,
            cache_dir,
            target_dir,
            marker,
            uid,
            active: false,
        };
        guard.refresh()?;
        guard.active = true;
        Ok(guard)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    /// Обновляет `last_used` после ошибки запуска или ожидания команды, затем
    /// освобождает блокировку. Ошибка маркера возвращается вызывающему коду;
    /// блокировка освобождается при любом результате.
    pub fn finish(mut self) -> Result<(), String> {
        self.active = false;
        self.refresh()
    }

    fn refresh(&mut self) -> Result<(), String> {
        let pinned = self
            .directory
            .metadata()
            .map_err(|error| error.to_string())?;
        let current = fs::symlink_metadata(&self.cache_dir).map_err(|error| error.to_string())?;
        if !current.is_dir()
            || current.file_type().is_symlink()
            || current.uid() != self.uid
            || current.mode() & 0o077 != 0
            || current.dev() != pinned.dev()
            || current.ino() != pinned.ino()
        {
            return Err("каталог кэша изменился во время управляемой команды".into());
        }
        let mut marker =
            read_marker_at(&self.directory, self.uid).map_err(|error| error.to_string())?;
        if marker.schema != self.marker.schema
            || marker.repository_root != self.marker.repository_root
            || marker.cache_id != self.marker.cache_id
            || marker.target_dir != self.marker.target_dir
            || marker.created_unix_seconds != self.marker.created_unix_seconds
        {
            return Err("маркер владения кэшем изменился во время управляемой команды".into());
        }
        marker.last_used_unix_seconds = marker.last_used_unix_seconds.max(unix_now());
        write_marker_at(&self.directory, &marker).map_err(|error| error.to_string())
    }
}

impl Drop for CacheRunGuard {
    fn drop(&mut self) {
        if self.active {
            // Явный `finish` возвращает ошибки; `Drop` старается сохранить маркер
            // при раскрутке стека.
            let _ = self.refresh();
        }
    }
}

fn validate_managed_target_path(cache_dir: &Path, target: &Path, uid: u32) -> Result<(), String> {
    let cache_device = fs::symlink_metadata(cache_dir)
        .map_err(|error| error.to_string())?
        .dev();
    let mut path = cache_dir.to_path_buf();
    for component in target.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == uid
                    && metadata.dev() == cache_device => {}
            Ok(_) => {
                return Err(
                    "управляемый каталог `target` содержит символическую ссылку, каталог другого владельца или границу точки монтирования".into(),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "не удалось проверить управляемый каталог `target`: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ExternalCacheInventory {
    pub fn empty() -> Self {
        Self {
            root: PathBuf::new(),
            allocated_bytes: 0,
            managed_entries: 0,
            unknown_entries: 0,
            candidates: Vec::new(),
        }
    }
}

impl ExternalCacheCandidate {
    pub fn eligible_for_cleanup(&self, age: Duration, now: u64) -> bool {
        self.ownership == "project-owned"
            && self.live == Some(false)
            && self.last_used_unix_seconds.is_some_and(|last_used| {
                last_used <= now && now.saturating_sub(last_used) >= age.as_secs()
            })
            && self.target_dir.is_some()
            && self.measurement.is_some()
            && self.error.is_none()
    }
}

/// Владение внешним путём кэша подтверждается маркером, совпадающим с корнем
/// текущей рабочей области. Немаркированные записи измеряются, но не становятся
/// целью сборщика мусора.
pub fn inspect_external_caches(
    repository_root: &Path,
    cache_root: &Path,
    min_age: Duration,
) -> io::Result<ExternalCacheInventory> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let repository_root = repository_root.canonicalize()?;
    let root_meta = match fs::symlink_metadata(cache_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "корень внешних кэшей должен быть обычным каталогом",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExternalCacheInventory {
                root: cache_root.to_path_buf(),
                allocated_bytes: 0,
                managed_entries: 0,
                unknown_entries: 0,
                candidates: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let current_uid = fs::metadata("/proc/self")?.uid();
    if root_meta.uid() != current_uid || root_meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "корень внешних кэшей должен принадлежать текущему UID и быть приватным",
        ));
    }
    let canonical_cache_root = cache_root.canonicalize()?;
    let mut children = Vec::new();
    for entry in fs::read_dir(&canonical_cache_root)? {
        children.push(entry?.path());
    }
    children.sort();
    let mut candidates = Vec::new();
    let mut allocated_bytes = 0_u64;
    let mut managed_entries = 0;
    let mut unknown_entries = 0;
    let min_used = now.saturating_sub(min_age.as_secs());
    for path in children {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                unknown_entries += 1;
                candidates.push(unknown_candidate(
                    &path,
                    &format!("не удалось прочитать метаданные объекта: {error}"),
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            unknown_entries += 1;
            candidates.push(unknown_candidate(
                &path,
                "объект не является обычным каталогом кэша; он не удаляется",
            ));
            continue;
        }
        if metadata.uid() != current_uid || metadata.mode() & 0o077 != 0 {
            unknown_entries += 1;
            candidates.push(unknown_candidate(
                &path,
                "каталог принадлежит другому UID или права доступа не ограничены владельцем",
            ));
            continue;
        }
        let id = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        match read_marker(&path, current_uid) {
            Ok(marker)
                if marker.schema == 1
                    && marker.repository_root == repository_root
                    && marker.cache_id == id
                    && valid_relative_target(&marker.target_dir)
                    && marker.created_unix_seconds <= now
                    && marker.last_used_unix_seconds <= now =>
            {
                let target_dir = path.join(&marker.target_dir);
                let used_age = now.saturating_sub(marker.last_used_unix_seconds);
                let mut candidate = ExternalCacheCandidate {
                    id,
                    path: path.clone(),
                    target_dir: Some(target_dir.clone()),
                    allocated_bytes: 0,
                    bytes_after: None,
                    bytes_freed: 0,
                    last_used_unix_seconds: Some(marker.last_used_unix_seconds),
                    age_seconds: Some(used_age),
                    ownership: "project-owned",
                    live: Some(false),
                    action: "keep",
                    reason: if marker.last_used_unix_seconds > min_used {
                        "кэш использовался недавно; минимальный срок хранения ещё не истёк".into()
                    } else {
                        "маркер подтверждает кэш этой рабочей области".into()
                    },
                    error: None,
                    measurement: None,
                };
                match validate_cargo_target(&target_dir) {
                    Ok(()) => match measure_path(&target_dir) {
                        Ok(measurement) => {
                            candidate.allocated_bytes = measurement.allocated_bytes;
                            candidate.measurement = Some(measurement.clone());
                            match target_process_check(&target_dir, &repository_root) {
                                ProcessCheck::Clear => (),
                                ProcessCheck::Busy {
                                    pid,
                                    process,
                                    reference,
                                } => {
                                    candidate.live = Some(true);
                                    candidate.reason =
                                        format!("активен {process} PID {pid}: {reference}");
                                }
                                ProcessCheck::Unknown { reason } => {
                                    candidate.live = None;
                                    candidate.error = Some(reason);
                                }
                            }
                            if measurement.mount_boundaries != 0 || !measurement.errors.is_empty() {
                                candidate.error = Some(format!(
                                    "дерево содержит границы точек монтирования или ошибки: {} границ, {} ошибок",
                                    measurement.mount_boundaries,
                                    measurement.errors.len()
                                ));
                            }
                            allocated_bytes =
                                allocated_bytes.saturating_add(candidate.allocated_bytes);
                        }
                        Err(error) => candidate.error = Some(error.to_string()),
                    },
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        candidate.allocated_bytes = 0;
                    }
                    Err(error) => candidate.error = Some(error.to_string()),
                }
                managed_entries += 1;
                candidates.push(candidate);
            }
            Ok(_) => {
                unknown_entries += 1;
                candidates.push(unknown_candidate(
                    &path,
                    "маркер владения не совпал с корнем рабочей области",
                ));
            }
            Err(error) => {
                unknown_entries += 1;
                candidates.push(unknown_candidate(&path, &error.to_string()));
            }
        }
    }
    Ok(ExternalCacheInventory {
        root: canonical_cache_root,
        allocated_bytes,
        managed_entries,
        unknown_entries,
        candidates,
    })
}

/// Создаёт сведения о происхождении только в новом пустом каталоге кэша проекта.
pub fn initialize_build_cache(
    repository_root: &Path,
    cache_root: &Path,
    id: &str,
) -> Result<PathBuf, String> {
    if !safe_cache_id(id) {
        return Err("идентификатор кэша должен состоять из букв ASCII, цифр, '-' или '_'".into());
    }
    let root = cache_root.join(id);
    ensure_private_directory(cache_root)?;
    match fs::create_dir(&root) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return resolve_build_cache(repository_root, cache_root, id)
                .map(|(cache_dir, _)| cache_dir);
        }
        Err(error) => return Err(format!("не удалось создать каталог кэша: {error}")),
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("не удалось ограничить права доступа к каталогу кэша: {error}"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let marker = BuildCacheMarker {
        schema: 1,
        repository_root: repository_root.canonicalize().map_err(|e| e.to_string())?,
        cache_id: id.to_owned(),
        target_dir: PathBuf::from("target"),
        created_unix_seconds: now,
        last_used_unix_seconds: now,
    };
    write_marker(&root, &marker).map_err(|error| error.to_string())?;
    root.canonicalize().map_err(|error| error.to_string())
}

pub fn resolve_build_cache(
    repository_root: &Path,
    cache_root: &Path,
    id: &str,
) -> Result<(PathBuf, PathBuf), String> {
    if !safe_cache_id(id) {
        return Err("идентификатор кэша должен состоять из букв ASCII, цифр, '-' или '_'".into());
    }
    let repository_root = repository_root
        .canonicalize()
        .map_err(|error| format!("не удалось определить корень рабочей области: {error}"))?;
    let cache_dir = cache_root.join(id);
    let uid = fs::metadata("/proc/self")
        .map_err(|error| error.to_string())?
        .uid();
    let metadata = fs::symlink_metadata(&cache_dir).map_err(|error| {
        format!(
            "не удалось проверить каталог кэша {}: {error}",
            cache_dir.display()
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err("каталог кэша не является приватным каталогом текущего пользователя".into());
    }
    let cache_dir = cache_dir
        .canonicalize()
        .map_err(|error| format!("не удалось определить путь к каталогу кэша: {error}"))?;
    let marker = read_marker(&cache_dir, uid).map_err(|error| error.to_string())?;
    if marker.schema != 1
        || marker.repository_root != repository_root
        || marker.cache_id != id
        || !valid_relative_target(&marker.target_dir)
    {
        return Err(
            "маркер владения кэшем не совпадает с рабочей областью или идентификатором кэша".into(),
        );
    }
    Ok((cache_dir.clone(), cache_dir.join(marker.target_dir)))
}

/// Обновляет время последнего использования в маркере перед запуском процесса,
/// создающего данные в управляемом кэше.
pub fn touch_build_cache(path: &Path) -> Result<(), String> {
    let uid = fs::metadata("/proc/self")
        .map_err(|error| error.to_string())?
        .uid();
    let directory = open_private_directory(path, uid).map_err(|error| error.to_string())?;
    let mut marker = read_marker_at(&directory, uid).map_err(|error| error.to_string())?;
    marker.last_used_unix_seconds = marker.last_used_unix_seconds.max(unix_now());
    write_marker_at(&directory, &marker).map_err(|error| error.to_string())
}

pub fn cleanup_plan(
    inventory: &mut ExternalCacheInventory,
    hard_limit_bytes: u64,
    goal_bytes: u64,
    min_age: Duration,
    now: u64,
) -> Vec<String> {
    if inventory.allocated_bytes <= hard_limit_bytes {
        return Vec::new();
    }
    let mut eligible = inventory
        .candidates
        .iter()
        .filter(|candidate| candidate.eligible_for_cleanup(min_age, now))
        .map(|candidate| {
            (
                candidate.last_used_unix_seconds.unwrap_or_default(),
                candidate.id.clone(),
            )
        })
        .collect::<Vec<_>>();
    eligible.sort();
    for candidate in &mut inventory.candidates {
        if candidate.ownership != "project-owned" || candidate.allocated_bytes == 0 {
            continue;
        }
        if candidate.live != Some(false) {
            candidate.action = "deferred";
            candidate.reason = if candidate.live == Some(true) {
                "активный процесс использует кэш; очистка отложена".into()
            } else {
                "нельзя доказать отсутствие активного процесса; очистка отложена".into()
            };
        } else if candidate.error.is_some() {
            candidate.action = "error";
            continue;
        } else if !candidate.eligible_for_cleanup(min_age, now) {
            candidate.action = "deferred";
            candidate.reason =
                "минимальный срок хранения кэша ещё не истёк; очистка отложена".into();
        }
    }
    let mut remaining = inventory.allocated_bytes;
    let mut selected = Vec::new();
    for (_, id) in eligible {
        if remaining <= goal_bytes {
            break;
        }
        let Some(candidate) = inventory
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == id)
        else {
            continue;
        };
        candidate.action = "delete";
        candidate.reason = "общий размер внешних кэшей, принадлежащих проекту, превысил жёсткий предел; очищается самый старый кэш".into();
        remaining = remaining.saturating_sub(candidate.allocated_bytes);
        selected.push(candidate.id.clone());
    }
    selected
}

pub fn clean_cache(repository_root: &Path, target_dir: &Path) -> Result<(String, String), String> {
    validate_cargo_target(target_dir).map_err(|error| {
        format!("каталог `target` кэша не подтверждён маркером `CACHEDIR.TAG`: {error}")
    })?;
    let workspace = CargoWorkspace {
        root: repository_root.to_path_buf(),
        manifest: repository_root.join("Cargo.toml"),
        target_dir: target_dir.to_path_buf(),
        build_dir: Some(target_dir.to_path_buf()),
        measured_dirs: vec![target_dir.to_path_buf()],
    };
    cargo_clean_external(&workspace)
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
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
            fs::create_dir_all(path)
                .map_err(|error| format!("не удалось создать {}: {error}", path.display()))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("не удалось проверить {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!("{} заменён во время создания", path.display()));
            }
            let uid = fs::metadata("/proc/self")
                .map_err(|error| error.to_string())?
                .uid();
            if metadata.uid() != uid {
                return Err(format!("{} принадлежит другому UID", path.display()));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("не удалось закрыть права {}: {error}", path.display()))?;
        }
        Err(error) => return Err(format!("не удалось проверить {}: {error}", path.display())),
    }
    Ok(())
}

fn open_private_directory(directory: &Path, uid: u32) -> io::Result<File> {
    let file = File::from(rustix::fs::open(
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "каталог кэша не является приватным каталогом текущего UID",
        ));
    }
    Ok(file)
}

fn read_marker(directory: &Path, uid: u32) -> io::Result<BuildCacheMarker> {
    let directory = open_private_directory(directory, uid)?;
    read_marker_at(&directory, uid)
}

fn read_marker_at(directory: &File, uid: u32) -> io::Result<BuildCacheMarker> {
    let marker_fd = openat(
        directory,
        MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let file = File::from(marker_fd);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MARKER_LIMIT
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "небезопасный маркер внешнего кэша",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT + 1).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn write_marker(directory: &Path, marker: &BuildCacheMarker) -> io::Result<()> {
    let uid = fs::metadata("/proc/self")?.uid();
    let directory = open_private_directory(directory, uid)?;
    write_marker_at(&directory, marker)
}

fn write_marker_at(directory: &File, marker: &BuildCacheMarker) -> io::Result<()> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = format!("{MARKER}.tmp-{}-{suffix}", std::process::id());
    let mut file = File::from(openat(
        directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        serde_json::to_writer(&mut file, marker).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        renameat(directory, temporary.as_str(), directory, MARKER)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

fn valid_relative_target(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && path.components().count() <= 4
}

fn safe_cache_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn unknown_candidate(path: &Path, reason: &str) -> ExternalCacheCandidate {
    let (bytes, error) = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            match measure_path(path) {
                Ok(measurement) => {
                    let error =
                        if measurement.errors.is_empty() && measurement.mount_boundaries == 0 {
                            None
                        } else {
                            Some(format!(
                                "неполное измерение: {} границ точек монтирования, {} ошибок",
                                measurement.mount_boundaries,
                                measurement.errors.len()
                            ))
                        };
                    (measurement.allocated_bytes, error)
                }
                Err(error) => (0, Some(error.to_string())),
            }
        }
        Ok(metadata) => (metadata.blocks().saturating_mul(512), None),
        Err(error) => (0, Some(error.to_string())),
    };
    ExternalCacheCandidate {
        id: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.to_path_buf(),
        target_dir: None,
        allocated_bytes: bytes,
        bytes_after: None,
        bytes_freed: 0,
        last_used_unix_seconds: None,
        age_seconds: None,
        ownership: "unknown",
        live: None,
        action: "keep",
        reason: reason.to_owned(),
        error,
        measurement: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::process::{Command, Stdio};
    use std::thread;

    const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n";

    struct Fixture {
        _owner: TempWorkspace,
        repository: PathBuf,
        cache_root: PathBuf,
        cache_dir: PathBuf,
        target: PathBuf,
    }

    fn fixture(id: &str) -> Fixture {
        let owner = TempWorkspace::create("repository-maintenance-cache-test").unwrap();
        let repository = owner.path().join("checkout");
        fs::create_dir_all(&repository).unwrap();
        fs::write(
            repository.join("Cargo.toml"),
            "[workspace]\nmembers = []\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::create_dir_all(repository.join("src")).unwrap();
        fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        fs::write(repository.join("Cargo.lock"), "version = 3\n").unwrap();
        let cache_root = owner.path().join("managed-caches");
        let cache_dir = initialize_build_cache(&repository, &cache_root, id).unwrap();
        let target = cache_dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE).unwrap();
        fs::write(target.join("artifact"), [0x35_u8; 4096]).unwrap();
        Fixture {
            _owner: owner,
            repository,
            cache_root,
            cache_dir,
            target,
        }
    }

    fn set_last_used(path: &Path, timestamp: u64) {
        let marker_path = path.join(MARKER);
        let mut marker: BuildCacheMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.last_used_unix_seconds = timestamp;
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
    }

    fn begin_fixture(fixture: &Fixture, id: &str) -> (CacheRunGuard, PathBuf) {
        let lock_directory = fixture._owner.path().join("locks");
        let lock = MaintenanceLock::acquire_at(&lock_directory).unwrap();
        let guard =
            CacheRunGuard::begin(&fixture.repository, &fixture.cache_root, id, lock).unwrap();
        (guard, lock_directory)
    }

    #[test]
    fn managed_child_holds_lock_and_updates_marker_before_and_after_wait() {
        let fixture = fixture("producer");
        set_last_used(&fixture.cache_dir, 1);
        let (guard, lock_directory) = begin_fixture(&fixture, "producer");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        assert!(
            read_marker(&fixture.cache_dir, uid)
                .unwrap()
                .last_used_unix_seconds
                > 1
        );
        assert_eq!(guard.target_dir(), fixture.target);
        assert_eq!(guard.cache_dir(), fixture.cache_dir);
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let other_lock_directory = lock_directory.clone();
        let other = thread::spawn(move || {
            MaintenanceLock::acquire_at(&other_lock_directory)
                .err()
                .unwrap()
                .kind()
        });
        assert_eq!(other.join().unwrap(), io::ErrorKind::WouldBlock);
        assert!(child.try_wait().unwrap().is_none());
        set_last_used(&fixture.cache_dir, 1);
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        guard.finish().unwrap();
        assert!(
            read_marker(&fixture.cache_dir, uid)
                .unwrap()
                .last_used_unix_seconds
                > 1
        );
        assert!(MaintenanceLock::acquire_at(&lock_directory).is_ok());
    }

    #[test]
    fn managed_child_failure_and_signal_release_lock_after_marker_refresh() {
        let fixture = fixture("failed-producer");
        for killed in [false, true] {
            let (guard, lock_directory) = begin_fixture(&fixture, "failed-producer");
            let mut child = if killed {
                Command::new("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap()
            } else {
                Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap()
            };
            if killed {
                child.kill().unwrap();
            }
            assert!(!child.wait().unwrap().success());
            set_last_used(&fixture.cache_dir, 1);
            guard.finish().unwrap();
            let uid = fs::metadata("/proc/self").unwrap().uid();
            assert!(
                read_marker(&fixture.cache_dir, uid)
                    .unwrap()
                    .last_used_unix_seconds
                    > 1
            );
            assert!(MaintenanceLock::acquire_at(&lock_directory).is_ok());
        }
    }

    #[test]
    fn managed_guard_refreshes_marker_and_unlocks_on_early_return() {
        let fixture = fixture("drop-producer");
        let (guard, lock_directory) = begin_fixture(&fixture, "drop-producer");
        set_last_used(&fixture.cache_dir, 1);
        drop(guard);
        let uid = fs::metadata("/proc/self").unwrap().uid();
        assert!(
            read_marker(&fixture.cache_dir, uid)
                .unwrap()
                .last_used_unix_seconds
                > 1
        );
        assert!(MaintenanceLock::acquire_at(&lock_directory).is_ok());
    }

    #[test]
    fn managed_guard_refuses_unknown_cache_and_replaced_marker() {
        let fixture = fixture("owned-producer");
        let unknown = fixture.cache_root.join("unknown");
        fs::create_dir(&unknown).unwrap();
        fs::set_permissions(&unknown, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(unknown.join("keep"), b"foreign").unwrap();
        let lock_directory = fixture._owner.path().join("locks");
        let lock = MaintenanceLock::acquire_at(&lock_directory).unwrap();
        assert!(
            CacheRunGuard::begin(&fixture.repository, &fixture.cache_root, "unknown", lock)
                .is_err()
        );
        assert!(!unknown.join(MARKER).exists());
        assert_eq!(fs::read(unknown.join("keep")).unwrap(), b"foreign");
        let (guard, _) = begin_fixture(&fixture, "owned-producer");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let mut marker = read_marker(&fixture.cache_dir, uid).unwrap();
        marker.cache_id = "foreign-cache".into();
        write_marker(&fixture.cache_dir, &marker).unwrap();
        let before = fs::read(fixture.cache_dir.join(MARKER)).unwrap();
        assert!(guard.finish().is_err());
        assert_eq!(fs::read(fixture.cache_dir.join(MARKER)).unwrap(), before);
        assert!(MaintenanceLock::acquire_at(&lock_directory).is_ok());
    }

    #[test]
    fn managed_guard_refuses_target_symlink_before_updating_marker() {
        let fixture = fixture("linked-target");
        let outside = fixture._owner.path().join("outside-target");
        fs::rename(&fixture.target, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &fixture.target).unwrap();
        let before = fs::read(fixture.cache_dir.join(MARKER)).unwrap();
        let directory = fixture._owner.path().join("locks");
        let lock = MaintenanceLock::acquire_at(&directory).unwrap();
        assert!(
            CacheRunGuard::begin(
                &fixture.repository,
                &fixture.cache_root,
                "linked-target",
                lock
            )
            .is_err()
        );
        assert_eq!(fs::read(fixture.cache_dir.join(MARKER)).unwrap(), before);
        assert!(outside.join("artifact").is_file());
        assert!(MaintenanceLock::acquire_at(&directory).is_ok());
    }

    #[test]
    fn only_stale_marker_owned_external_targets_enter_cleanup_plan() {
        let fixture = fixture("stale-cache");
        let unknown = fixture.cache_root.join("unmarked");
        fs::create_dir(&unknown).unwrap();
        let unknown_target = unknown.join("target");
        fs::create_dir(&unknown_target).unwrap();
        fs::write(unknown_target.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE).unwrap();
        fs::write(unknown_target.join("artifact"), [0x45_u8; 4096]).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        set_last_used(&fixture.cache_dir, now.saturating_sub(3600));

        let mut inventory = inspect_external_caches(
            &fixture.repository,
            &fixture.cache_root,
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(inventory.managed_entries, 1);
        assert_eq!(inventory.unknown_entries, 1);
        assert!(inventory.allocated_bytes > 0);
        let candidate = inventory
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == "stale-cache")
            .unwrap();
        candidate.live = Some(false);
        candidate.error = None;
        let selected = cleanup_plan(&mut inventory, 1, 0, Duration::from_secs(60), now);
        assert_eq!(selected, ["stale-cache"]);
        assert_eq!(
            inventory
                .candidates
                .iter()
                .find(|candidate| candidate.id == "unmarked")
                .unwrap()
                .action,
            "keep"
        );
    }

    #[test]
    fn unknown_files_and_symlinks_are_visible_without_following_links() {
        let fixture = fixture("visible-unknowns");
        let unknown_file = fixture.cache_root.join("loose-file");
        fs::write(&unknown_file, [0x71_u8; 2048]).unwrap();
        let unknown_link = fixture.cache_root.join("linked-cache");
        std::os::unix::fs::symlink(&fixture.cache_dir, &unknown_link).unwrap();
        let link_blocks = fs::symlink_metadata(&unknown_link).unwrap().blocks() * 512;

        let inventory = inspect_external_caches(
            &fixture.repository,
            &fixture.cache_root,
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(inventory.managed_entries, 1);
        assert_eq!(inventory.unknown_entries, 2);
        let file = inventory
            .candidates
            .iter()
            .find(|candidate| candidate.id == "loose-file")
            .unwrap();
        assert!(file.allocated_bytes > 0);
        assert_eq!(file.ownership, "unknown");
        assert_eq!(file.action, "keep");
        let link = inventory
            .candidates
            .iter()
            .find(|candidate| candidate.id == "linked-cache")
            .unwrap();
        assert_eq!(link.allocated_bytes, link_blocks);
        assert_eq!(link.ownership, "unknown");
        assert_eq!(link.action, "keep");
    }

    #[test]
    fn live_external_target_is_deferred_even_when_over_limit() {
        let fixture = fixture("live-cache");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        set_last_used(&fixture.cache_dir, now.saturating_sub(3600));
        let mut child = Command::new("sleep")
            .arg("5")
            .current_dir(&fixture.target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(40));

        let mut inventory = inspect_external_caches(
            &fixture.repository,
            &fixture.cache_root,
            Duration::from_secs(60),
        )
        .unwrap();
        let selected = cleanup_plan(&mut inventory, 1, 0, Duration::from_secs(60), now);
        let candidate = inventory.candidates.first().unwrap();
        assert!(selected.is_empty());
        assert_eq!(candidate.action, "deferred");
        assert_ne!(candidate.live, Some(false));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn explicit_external_cargo_clean_preserves_unknown_sibling_cache() {
        let fixture = fixture("clean-cache");
        let sibling = fixture.cache_root.join("unmarked");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("keep-me"), b"foreign cache").unwrap();
        let before = measure_path(&fixture.target).unwrap().allocated_bytes;
        clean_cache(&fixture.repository, &fixture.target).unwrap();
        assert!(!fixture.target.exists());
        assert!(sibling.join("keep-me").exists());
        assert!(before > 0);
    }
}
