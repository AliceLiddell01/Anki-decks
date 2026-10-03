//! Владелец временного дерева и консервативная уборка собственных orphan runs.
//!
//! Все операции обхода и удаления закреплены на directory descriptors. Marker
//! подтверждает назначение, а UID, namespace, NOFOLLOW и TTL ограничивают уборку.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, StatxFlags, mkdirat, open, openat, statat, statx, unlinkat,
};
use serde::{Deserialize, Serialize};

const TEMP_ROOT: &str = "/tmp";
const NAMESPACE: &str = "anki-decks-work";
const MARKER: &str = ".anki-decks-owner.json";
const REPOSITORY: &str = "AliceLiddell01/Anki-decks";
const TOOL: &str = "asset-store";
const SCHEMA: u32 = 1;
const MARKER_LIMIT: u64 = 16 * 1024;
static STARTUP_GC: OnceLock<Result<(), (io::ErrorKind, String)>> = OnceLock::new();
pub const DEFAULT_ORPHAN_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipMarker {
    schema: u32,
    repository: String,
    tool: String,
    pid: u32,
    created_unix_ms: u64,
    run_id: String,
    purpose: String,
}

/// Объект обязан жить дольше всех пользователей `path()`.
#[derive(Debug)]
pub struct TempWorkspace {
    path: PathBuf,
    run_id: String,
    parent: File,
    directory: File,
    closed: bool,
}

impl TempWorkspace {
    /// Создаёт приватное дерево после startup GC (TTL — 24 часа).
    pub fn create(purpose: &str) -> io::Result<Self> {
        STARTUP_GC
            .get_or_init(|| {
                cleanup_orphans(DEFAULT_ORPHAN_MIN_AGE)
                    .map(|report| {
                        tracing::debug!(
                            removed = report.removed,
                            skipped = report.skipped,
                            errors = report.errors,
                            "startup temp cleanup"
                        );
                    })
                    .map_err(|error| (error.kind(), error.to_string()))
            })
            .as_ref()
            .map_err(|(kind, message)| io::Error::new(*kind, message.clone()))?;
        Self::create_under(Path::new(TEMP_ROOT), purpose)
    }

    fn create_under(temp_root: &Path, purpose: &str) -> io::Result<Self> {
        if purpose.is_empty() || purpose.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temp purpose должен содержать 1..4096 байт",
            ));
        }
        let uid = current_uid()?;
        let parent = namespace(temp_root, uid, true)?
            .ok_or_else(|| io::Error::other("не удалось создать temp namespace"))?;
        let mut random = [0_u8; 16];
        for _ in 0..32 {
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let run_id = format!("run-{}", crate::hashing::encode_lower_hex(random));
            match mkdirat(&parent, run_id.as_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    let directory = directory_at(&parent, OsStr::new(&run_id))?;
                    let owner = Self {
                        path: temp_root.join(NAMESPACE).join(&run_id),
                        run_id: run_id.clone(),
                        parent,
                        directory,
                        closed: false,
                    };
                    let marker = OwnershipMarker {
                        schema: SCHEMA,
                        repository: REPOSITORY.into(),
                        tool: TOOL.into(),
                        pid: std::process::id(),
                        created_unix_ms: unix_ms()?,
                        run_id,
                        purpose: purpose.into(),
                    };
                    let fd = openat(
                        &owner.directory,
                        MARKER,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    )?;
                    let mut file = File::from(fd);
                    serde_json::to_writer(&mut file, &marker).map_err(io::Error::other)?;
                    file.write_all(b"\n")?;
                    file.sync_all()?;
                    return Ok(owner);
                }
                Err(error) if error == rustix::io::Errno::EXIST => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "исчерпаны попытки создания temp run",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ошибка cleanup возвращается вызывающему коду; Drop повторяет best effort.
    pub fn close(mut self) -> io::Result<()> {
        self.remove()?;
        self.closed = true;
        Ok(())
    }

    fn remove(&self) -> io::Result<()> {
        verify_identity(&self.parent, OsStr::new(&self.run_id), &self.directory)?;
        remove_contents(&self.directory)?;
        verify_identity(&self.parent, OsStr::new(&self.run_id), &self.directory)?;
        unlinkat(&self.parent, self.run_id.as_str(), AtFlags::REMOVEDIR).map_err(Into::into)
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if !self.closed
            && let Err(error) = self.remove()
        {
            tracing::warn!(path = %self.path.display(), %error, "temp workspace cleanup failed");
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub removed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub removed_bytes: u64,
    pub entries: Vec<GcEntry>,
}

#[derive(Debug, Serialize)]
pub struct GcEntry {
    pub path: PathBuf,
    pub outcome: String,
    pub reason: String,
    pub bytes: u64,
}

/// Удаляет только старые, подтверждённые marker orphan runs мёртвых PID.
/// Ошибки отдельных деревьев отражены в `errors` и `entries`, затем GC продолжается.
pub fn cleanup_orphans(min_age: Duration) -> io::Result<GcReport> {
    cleanup_under(Path::new(TEMP_ROOT), min_age)
}

fn cleanup_under(temp_root: &Path, min_age: Duration) -> io::Result<GcReport> {
    let min_age = min_age.max(DEFAULT_ORPHAN_MIN_AGE);
    let mut report = GcReport::default();
    let uid = current_uid()?;
    let Some(parent) = namespace(temp_root, uid, false)? else {
        return Ok(report);
    };
    let now = unix_ms()?;
    for name in names(&parent)? {
        let path = temp_root.join(NAMESPACE).join(&name);
        let result = inspect_orphan(&parent, &name, uid, now, min_age);
        let (outcome, reason, bytes) = match result {
            Ok(Inspection::Skip(reason)) => {
                report.skipped += 1;
                ("skipped", reason, 0)
            }
            Ok(Inspection::Delete(directory, bytes)) => {
                match verify_identity(&parent, &name, &directory)
                    .and_then(|()| remove_contents(&directory))
                    .and_then(|()| verify_identity(&parent, &name, &directory))
                    .and_then(|()| {
                        unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(io::Error::from)
                    }) {
                    Ok(()) => {
                        report.removed += 1;
                        report.removed_bytes = report.removed_bytes.saturating_add(bytes);
                        ("removed", "старый owned orphan".into(), bytes)
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        report.skipped += 1;
                        (
                            "skipped",
                            "дерево уже удалено параллельной уборкой".into(),
                            0,
                        )
                    }
                    Err(error) => {
                        report.errors += 1;
                        ("error", error.to_string(), 0)
                    }
                }
            }
            Err(error) => {
                report.skipped += 1;
                ("skipped", error.to_string(), 0)
            }
        };
        tracing::debug!(path = %path.display(), outcome, %reason, bytes, "orphan temp cleanup");
        report.entries.push(GcEntry {
            path,
            outcome: outcome.into(),
            reason,
            bytes,
        });
    }
    Ok(report)
}

enum Inspection {
    Skip(String),
    Delete(File, u64),
}

fn inspect_orphan(
    parent: &File,
    name: &OsStr,
    uid: u32,
    now: u64,
    min_age: Duration,
) -> io::Result<Inspection> {
    let Some(run_id) = name.to_str().filter(|name| valid_run_id(name)) else {
        return Ok(Inspection::Skip("чужой run prefix".into()));
    };
    let directory = directory_at(parent, name)?;
    let metadata = directory.metadata()?;
    same_mount(parent, &directory)?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Ok(Inspection::Skip("чужой UID или неприватный каталог".into()));
    }
    let fd = openat(
        &directory,
        MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = File::from(fd);
    let marker_metadata = file.metadata()?;
    if !marker_metadata.is_file()
        || marker_metadata.uid() != uid
        || marker_metadata.len() > MARKER_LIMIT
        || marker_metadata.nlink() != 1
        || marker_metadata.mode() & 0o022 != 0
    {
        return Ok(Inspection::Skip("небезопасный ownership marker".into()));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MARKER_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    let marker: OwnershipMarker = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if marker.schema != SCHEMA
        || marker.repository != REPOSITORY
        || marker.tool != TOOL
        || marker.run_id != run_id
        || marker.purpose.is_empty()
        || marker.pid == 0
    {
        return Ok(Inspection::Skip("чужой или неподдерживаемый marker".into()));
    }
    if marker.pid == std::process::id()
        || fs::symlink_metadata(format!("/proc/{}", marker.pid)).is_ok()
    {
        return Ok(Inspection::Skip("текущий или живой PID".into()));
    }
    // Любая ошибка проверки PID кроме NotFound — сомнение, а значит skip.
    if fs::symlink_metadata(format!("/proc/{}", marker.pid))
        .is_err_and(|error| error.kind() != io::ErrorKind::NotFound)
    {
        return Ok(Inspection::Skip("нельзя доказать отсутствие PID".into()));
    }
    let age_ms = u64::try_from(min_age.as_millis()).unwrap_or(u64::MAX);
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    if marker.created_unix_ms > now
        || now - marker.created_unix_ms < age_ms
        || modified > u128::from(now.saturating_sub(age_ms))
    {
        return Ok(Inspection::Skip("свежий run или время в будущем".into()));
    }
    let size = inspect_tree(&directory, uid)?;
    verify_identity(parent, name, &directory)?;
    Ok(Inspection::Delete(directory, size))
}

fn valid_run_id(name: &str) -> bool {
    name.len() == 36
        && name.starts_with("run-")
        && name[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn current_uid() -> io::Result<u32> {
    Ok(fs::metadata("/proc/self")?.uid())
}
fn unix_ms() -> io::Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    )
    .map_err(io::Error::other)
}

fn namespace(temp_root: &Path, uid: u32, create: bool) -> io::Result<Option<File>> {
    // NOFOLLOW applies to the root too; no configurable absolute escaped run paths.
    let temp = File::from(open(
        temp_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if create {
        match mkdirat(&temp, NAMESPACE, Mode::from_raw_mode(0o700)) {
            Ok(()) => (),
            Err(error) if error == rustix::io::Errno::EXIST => (),
            Err(error) => return Err(error.into()),
        }
    }
    let directory = match directory_at(&temp, OsStr::new(NAMESPACE)) {
        Ok(directory) => directory,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = directory.metadata()?;
    same_mount(&temp, &directory)?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "temp namespace имеет чужой UID или небезопасные permissions",
        ));
    }
    // canonicalization не используется как доказательство: fd остаётся boundary.
    if fs::canonicalize(temp_root.join(NAMESPACE))? != fs::canonicalize(temp_root)?.join(NAMESPACE)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "temp namespace выходит за root",
        ));
    }
    Ok(Some(directory))
}

fn directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    Ok(File::from(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            entries.push(OsStr::from_bytes(name).to_owned());
        }
    }
    entries.sort();
    Ok(entries)
}

fn verify_identity(parent: &File, name: &OsStr, directory: &File) -> io::Result<()> {
    let current = directory_at(parent, name)?;
    let before = directory.metadata()?;
    let after = current.metadata()?;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "temp directory подменён",
        ));
    }
    Ok(())
}

// Mount id различает bind mounts даже на том же device. Если идентичность
// mount недоступна, GC отказывается от обхода такого дерева.
fn same_mount(parent: &File, child: &File) -> io::Result<()> {
    let before = statx(parent, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    let after = statx(child, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if before.stx_mask & StatxFlags::MNT_ID.bits() == 0
        || after.stx_mask & StatxFlags::MNT_ID.bits() == 0
        || before.stx_mnt_id != after.stx_mnt_id
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "mount boundary или неизвестный mount id",
        ));
    }
    Ok(())
}

fn inspect_tree(directory: &File, uid: u32) -> io::Result<u64> {
    let mut bytes = 0_u64;
    for name in names(directory)? {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        if stat.st_uid != uid
            || stat.st_mode & 0o022 != 0
            || stat.st_dev != directory.metadata()?.dev()
            || kind == rustix::fs::FileType::Symlink
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "дерево содержит чужой UID, mount или symlink",
            ));
        }
        if kind == rustix::fs::FileType::Directory {
            let child = directory_at(directory, &name)?;
            same_mount(directory, &child)?;
            bytes = bytes.saturating_add(inspect_tree(&child, uid)?);
        } else if kind == rustix::fs::FileType::RegularFile {
            bytes = bytes.saturating_add(u64::try_from(stat.st_size).unwrap_or(0));
        } else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "дерево содержит special file",
            ));
        }
    }
    Ok(bytes)
}

// Измерение считает собственные regular files, включая деревья с symlink,
// но сам symlink и чужой mount никогда не обходит.
fn measure_tree(directory: &File, uid: u32) -> io::Result<u64> {
    let mut bytes = 0_u64;
    let device = directory.metadata()?.dev();
    for name in names(directory)? {
        let stat = match statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => return Err(error.into()),
        };
        if stat.st_uid != uid || stat.st_dev != device {
            continue;
        }
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Directory => {
                let child = match directory_at(directory, &name) {
                    Ok(child) => child,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if same_mount(directory, &child).is_err() {
                    continue;
                }
                match measure_tree(&child, uid) {
                    Ok(child_bytes) => bytes = bytes.saturating_add(child_bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
            }
            rustix::fs::FileType::RegularFile => {
                bytes = bytes.saturating_add(u64::try_from(stat.st_size).unwrap_or(0));
            }
            _ => (),
        }
    }
    Ok(bytes)
}

fn remove_contents(directory: &File) -> io::Result<()> {
    let mut entries = names(directory)?;
    // До удаления последнего файла сохраняем marker для повторной уборки при ошибке.
    entries.sort_by_key(|name| name == OsStr::new(MARKER));
    for name in entries {
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory {
            let child = directory_at(directory, &name)?;
            if same_mount(directory, &child).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "удаление mount запрещено",
                ));
            }
            remove_contents(&child)?;
            verify_identity(directory, &name, &child)?;
            unlinkat(directory, &name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(directory, &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, Serialize)]
pub struct TempSnapshot {
    pub directories: usize,
    pub bytes: u64,
    pub paths: Vec<PathBuf>,
}

/// Только измерение: сомнительные entries не обходятся и не удаляются.
pub fn snapshot() -> io::Result<TempSnapshot> {
    snapshot_under(Path::new(TEMP_ROOT))
}

fn snapshot_under(root: &Path) -> io::Result<TempSnapshot> {
    let mut snapshot = TempSnapshot::default();
    let uid = current_uid()?;
    if let Some(parent) = namespace(root, uid, false)? {
        for name in names(&parent)? {
            if name.to_str().is_some_and(valid_run_id) {
                measure_entry(
                    &parent,
                    &name,
                    root.join(NAMESPACE).join(&name),
                    uid,
                    &mut snapshot,
                )?;
            }
        }
    }
    let root_file = root_directory(root)?;
    for name in names(&root_file)? {
        if name.to_str().is_some_and(|name| legacy_pid(name).is_some()) {
            measure_entry(&root_file, &name, root.join(&name), uid, &mut snapshot)?;
        }
    }
    snapshot.paths.sort();
    Ok(snapshot)
}

fn measure_entry(
    parent: &File,
    name: &OsStr,
    path: PathBuf,
    uid: u32,
    snapshot: &mut TempSnapshot,
) -> io::Result<()> {
    let directory = match directory_at(parent, name) {
        Ok(directory) => directory,
        Err(_) => return Ok(()), // Недоказанное или уже исчезнувшее дерево.
    };
    if directory.metadata()?.uid() != uid || same_mount(parent, &directory).is_err() {
        return Ok(());
    }
    let bytes = match measure_tree(&directory, uid)
        .and_then(|bytes| verify_identity(parent, name, &directory).map(|()| bytes))
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    snapshot.directories += 1;
    snapshot.paths.push(path);
    snapshot.bytes = snapshot.bytes.saturating_add(bytes);
    Ok(())
}

/// Одноразовая миграция строго известных старых basename forms без marker.
/// Вызывается явно: fixture startup GC не удаляет legacy trees автоматически.
/// Для legacy TTL всегда не меньше 24 часов; известный живой PID блокирует удаление.
pub fn cleanup_legacy_orphans(min_age: Duration) -> io::Result<GcReport> {
    cleanup_legacy_under(Path::new(TEMP_ROOT), min_age.max(DEFAULT_ORPHAN_MIN_AGE))
}

fn cleanup_legacy_under(root: &Path, min_age: Duration) -> io::Result<GcReport> {
    let uid = current_uid()?;
    let parent = root_directory(root)?;
    let mut report = GcReport::default();
    for name in names(&parent)? {
        let Some(pid) = name.to_str().and_then(legacy_pid) else {
            continue;
        };
        let path = root.join(&name);
        let inspection = (|| {
            let directory = directory_at(&parent, &name)?;
            let metadata = directory.metadata()?;
            same_mount(&parent, &directory)?;
            if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "чужой UID или writable legacy tree",
                ));
            }
            if let Some(pid) = pid {
                match fs::symlink_metadata(format!("/proc/{pid}")) {
                    Ok(_) => return Err(io::Error::other("legacy PID жив")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
            }
            if metadata.modified()?.elapsed().map_err(io::Error::other)? < min_age {
                return Err(io::Error::other("свежий legacy tree"));
            }
            let bytes = inspect_tree(&directory, uid)?;
            Ok((directory, bytes))
        })();
        let (outcome, reason, bytes) = match inspection {
            Err(error) => {
                report.skipped += 1;
                ("skipped", error.to_string(), 0)
            }
            Ok((directory, bytes)) => {
                match verify_identity(&parent, &name, &directory)
                    .and_then(|()| remove_contents(&directory))
                    .and_then(|()| verify_identity(&parent, &name, &directory))
                    .and_then(|()| {
                        unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(io::Error::from)
                    }) {
                    Ok(()) => {
                        report.removed += 1;
                        report.removed_bytes = report.removed_bytes.saturating_add(bytes);
                        (
                            "removed",
                            "старое подтверждённое legacy basename".into(),
                            bytes,
                        )
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        report.skipped += 1;
                        (
                            "skipped",
                            "дерево уже удалено параллельной уборкой".into(),
                            0,
                        )
                    }
                    Err(error) => {
                        report.errors += 1;
                        ("error", error.to_string(), 0)
                    }
                }
            }
        };
        tracing::info!(path = %path.display(), outcome, %reason, bytes, "legacy temp cleanup");
        report.entries.push(GcEntry {
            path,
            outcome: outcome.into(),
            reason,
            bytes,
        });
    }
    tracing::info!(
        removed = report.removed,
        removed_bytes = report.removed_bytes,
        skipped = report.skipped,
        errors = report.errors,
        "legacy temp cleanup summary"
    );
    Ok(report)
}

fn root_directory(root: &Path) -> io::Result<File> {
    Ok(File::from(open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

/// Some(None): подтверждённый старый random basename без PID.
fn legacy_pid(name: &str) -> Option<Option<u32>> {
    if let Some(hex) = name.strip_prefix("kanji-")
        && hex.len() == 32
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Some(None);
    }
    for prefix in [
        "asset-store-pitch-cli-",
        "asset-store-pitch-batch-",
        "jpdb-live-acceptance-",
        "yarxi-live-acceptance-",
        "jpdb-session-stop-test-",
        "jpdb-failure-evidence-",
        "jpdb-report-durability-",
        "jpdb-acceptance-test-",
        "jpdb-acceptance-html-test-",
        "yarxi-acceptance-test-",
        "yarxi-acceptance-html-test-",
        "yarxi-acceptance-evidence-test-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, sequence) = suffix.split_once('-')?;
            let valid_sequence = if prefix.starts_with("asset-store-") {
                counter_value(sequence)
            } else {
                decimal(sequence) && sequence.parse::<u128>().is_ok()
            };
            return valid_sequence.then(|| parse_pid(pid)).flatten().map(Some);
        }
    }
    for prefix in [
        "asset-store-generic-batch-",
        "anki-kanji-batch-",
        "kanji-batch-cli-",
        "kanji-assets-source-boundary-",
        "asset-store-unit-",
        "asset-trust-",
        "kanji-assets-cli-",
        "pitch-cli-contract-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, counter) = suffix.split_once('-')?;
            return counter_value(counter)
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    if let Some(suffix) = name.strip_prefix("asset-store-diagnostics-") {
        let (pid, timestamp_counter) = suffix.split_once('-')?;
        let (timestamp, counter) = timestamp_counter.split_once('-')?;
        return (decimal(timestamp) && timestamp.parse::<u128>().is_ok() && counter_value(counter))
            .then(|| parse_pid(pid))
            .flatten()
            .map(Some);
    }
    for prefix in [
        "anki-repo-unit-",
        "anki-repo-test-",
        "asset-store-contract-",
    ] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (pid, label_counter) = suffix.split_once('-')?;
            let (label, counter) = label_counter.rsplit_once('-')?;
            return (safe_label(label) && counter_value(counter))
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    for prefix in ["domain-semantics-", "pitch-corpus-trust-"] {
        if let Some(suffix) = name.strip_prefix(prefix) {
            let (label_pid, counter) = suffix.rsplit_once('-')?;
            let (label, pid) = label_pid.rsplit_once('-')?;
            return (safe_label(label) && counter_value(counter))
                .then(|| parse_pid(pid))
                .flatten()
                .map(Some);
        }
    }
    let suffix = name.strip_prefix("anki-manifest-")?;
    let (label_pid, thread) = suffix.rsplit_once("-ThreadId(")?;
    let thread = thread.strip_suffix(')')?;
    if !decimal(thread) || thread.parse::<u64>().is_err() {
        return None;
    }
    let (label, pid) = label_pid.rsplit_once('-')?;
    const LABELS: &[&str] = &[
        "free",
        "foreign",
        "corrupt",
        "escape",
        "self-claim",
        "foreign-inside",
        "foreign-dir",
        "symlink-conflict",
        "symlink-conflict-outside",
        "symlink-at-file",
        "symlink-at-file-outside",
        "symlink-walk",
        "symlink-walk-outside",
        "symlink-out",
        "symlink-out-real",
        "repeat",
        "target-foreign",
        "interrupted",
        "parent-symlink",
        "parent-symlink-outside",
        "target-symlink",
        "abort",
        "missing-staged",
        "confined",
        "prune",
        "dir-on-file",
        "file-on-dir",
    ];
    if !LABELS.contains(&label) {
        return None;
    }
    parse_pid(pid).map(Some)
}

fn counter_value(value: &str) -> bool {
    decimal(value) && value.parse::<u64>().is_ok()
}
fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn decimal(value: &str) -> bool {
    !value.is_empty()
        && (value.len() == 1 || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn parse_pid(value: &str) -> Option<u32> {
    if decimal(value) {
        value.parse().ok().filter(|pid| *pid > 0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::os::unix::fs::symlink;

    fn sandbox() -> TempWorkspace {
        TempWorkspace::create_under(Path::new(TEMP_ROOT), "temp-workspace-unit").unwrap()
    }

    fn make_orphan(root: &Path, age: Duration) -> PathBuf {
        let mut owner = TempWorkspace::create_under(root, "gc-unit").unwrap();
        let marker_path = owner.path().join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker.pid = u32::MAX;
        marker.created_unix_ms = unix_ms().unwrap() - u64::try_from(age.as_millis()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        owner
            .directory
            .set_times(FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
        owner.closed = true; // Имитация SIGKILL без уничтожения тестового процесса.
        owner.path().to_path_buf()
    }

    #[test]
    fn scope_error_and_unwind_remove_owned_tree() {
        let path = {
            let owner = sandbox();
            fs::write(owner.path().join("payload"), b"data").unwrap();
            owner.path().to_path_buf()
        };
        assert!(!path.exists());
        fn fail_early(path: &mut Option<PathBuf>) -> io::Result<()> {
            let owner = sandbox();
            *path = Some(owner.path().to_path_buf());
            Err(io::Error::other("early error"))
        }
        let mut early_path = None;
        let result = fail_early(&mut early_path);
        assert!(result.is_err());
        assert!(!early_path.unwrap().exists());
        let mut panic_path = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let owner = sandbox();
            panic_path = Some(owner.path().to_path_buf());
            panic!("unwind");
        }));
        assert!(result.is_err());
        assert!(!panic_path.unwrap().exists());
    }

    #[test]
    fn explicit_close_and_parallel_creation() {
        let owners = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8).map(|_| scope.spawn(sandbox)).collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        let paths: std::collections::BTreeSet<_> = owners
            .iter()
            .map(|owner| owner.path().to_path_buf())
            .collect();
        assert_eq!(paths.len(), owners.len());
        for owner in owners {
            owner.close().unwrap();
        }
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn explicit_close_reports_replaced_directory_and_preserves_replacement() {
        let owner = sandbox();
        let path = owner.path().to_path_buf();
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("foreign"), b"preserve").unwrap();
        assert!(owner.close().is_err());
        assert!(path.join("foreign").exists());
        fs::remove_dir_all(path).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn old_orphan_removed_and_gc_is_idempotent() {
        let root = sandbox();
        let path = make_orphan(root.path(), Duration::from_secs(172800));
        fs::write(path.join("data"), b"payload").unwrap();
        File::open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let first = cleanup_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(first.removed, 1);
        assert!(first.removed_bytes >= 7);
        assert!(!path.exists());
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
    }

    #[test]
    fn fresh_current_live_foreign_marker_and_missing_marker_are_skipped() {
        let root = sandbox();
        let current = TempWorkspace::create_under(root.path(), "current").unwrap();
        let fresh = make_orphan(root.path(), Duration::from_secs(1));
        let live = make_orphan(root.path(), Duration::from_secs(172800));
        let live_marker_path = live.join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&live_marker_path).unwrap()).unwrap();
        marker.pid = 1;
        fs::write(&live_marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        let missing = make_orphan(root.path(), Duration::from_secs(172800));
        fs::remove_file(missing.join(MARKER)).unwrap();
        let foreign = make_orphan(root.path(), Duration::from_secs(172800));
        let foreign_marker_path = foreign.join(MARKER);
        let mut marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&foreign_marker_path).unwrap()).unwrap();
        marker.repository = "foreign".into();
        fs::write(foreign_marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        let foreign_prefix = root.path().join(NAMESPACE).join("other-application");
        fs::create_dir(&foreign_prefix).unwrap();
        let report = cleanup_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped, 6);
        for path in [
            current.path(),
            &fresh,
            &live,
            &missing,
            &foreign,
            &foreign_prefix,
        ] {
            assert!(path.exists());
        }
    }

    #[test]
    fn marker_schema_and_run_id_cannot_escape_root() {
        for (schema, run_id) in [(2, "invalid"), (SCHEMA, "../escape")] {
            let root = sandbox();
            let path = make_orphan(root.path(), Duration::from_secs(172800));
            let marker_path = path.join(MARKER);
            let mut marker: OwnershipMarker =
                serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
            marker.schema = schema;
            marker.run_id = run_id.into();
            fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
            assert_eq!(
                cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
                0
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn symlinks_and_namespace_escape_never_delete_targets() {
        let root = sandbox();
        let target = sandbox();
        fs::write(target.path().join("keep"), b"keep").unwrap();
        let path = make_orphan(root.path(), Duration::from_secs(172800));
        symlink(target.path(), path.join("escape")).unwrap();
        let link = root
            .path()
            .join(NAMESPACE)
            .join("run-11111111111111111111111111111111");
        symlink(target.path(), &link).unwrap();
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
        assert!(target.path().join("keep").exists());
        let escaped = sandbox();
        symlink(target.path(), escaped.path().join(NAMESPACE)).unwrap();
        assert!(cleanup_under(escaped.path(), Duration::ZERO).is_err());
        assert!(target.path().join("keep").exists());
    }

    #[test]
    fn legacy_parser_accepts_only_audited_basename_forms() {
        for name in [
            "anki-manifest-free-4294967295-ThreadId(17)",
            "anki-manifest-symlink-conflict-outside-4294967295-ThreadId(1)",
            "asset-store-pitch-cli-4294967295-9",
            "asset-store-pitch-batch-4294967295-9",
            "jpdb-live-acceptance-4294967295-1777777777777777777",
            "yarxi-live-acceptance-4294967295-1777777777777777777",
            "anki-repo-unit-4294967295-loader-bytes-0",
            "anki-repo-test-4294967295-cli-usage-0",
            "asset-store-generic-batch-4294967295-0",
            "anki-kanji-batch-4294967295-0",
            "kanji-batch-cli-4294967295-0",
            "kanji-assets-source-boundary-4294967295-0",
            "asset-store-unit-4294967295-0",
            "asset-trust-4294967295-0",
            "kanji-assets-cli-4294967295-0",
            "pitch-cli-contract-4294967295-0",
            "asset-store-diagnostics-4294967295-1777777777777777777-0",
            "asset-store-contract-4294967295-reopen-ingest-0",
            "domain-semantics-trust-automated-only-4294967295-0",
            "pitch-corpus-trust-gate-v4-approved-4294967295-0",
            "jpdb-session-stop-test-4294967295-1777777777777777777",
            "yarxi-acceptance-evidence-test-4294967295-1777777777777777777",
            "kanji-0123456789abcdef0123456789abcdef",
        ] {
            assert!(legacy_pid(name).is_some(), "{name}");
        }
        for name in [
            "foreign-app-1-2",
            "anki-manifest-custom-1-ThreadId(1)",
            "anki-manifest-free-0-ThreadId(1)",
            "anki-manifest-free-1-ThreadId(1)/escape",
            "asset-store-pitch-cli-1-2-more",
            "asset-store-pitch-batch-a-1",
            "kanji-0123456789ABCDEF0123456789ABCDEF",
            "kanji-0123456789abcdef0123456789abcde",
            "anki-repo-unit-1-../escape-0",
            "anki-repo-unit-1--0",
            "anki-repo-test-01-label-0",
            "asset-store-generic-batch-1-label-0",
            "asset-store-diagnostics-1-42-0-extra",
            "asset-store-contract-1-bad.label-0",
            "domain-semantics-label-0-0",
            "pitch-corpus-trust-label-1-18446744073709551616",
            "jpdb-session-stop-test-1-42-extra",
            "jpdb-live-acceptance-1-../escape",
        ] {
            assert!(legacy_pid(name).is_none(), "{name}");
        }
    }

    #[test]
    fn legacy_gc_skips_foreign_fresh_live_and_symlinks_and_is_idempotent() {
        let root = sandbox();
        let target = sandbox();
        fs::write(target.path().join("keep"), b"keep").unwrap();
        let old = root.path().join("asset-store-pitch-cli-4294967295-1");
        fs::create_dir(&old).unwrap();
        fs::write(old.join("data"), b"old payload").unwrap();
        File::open(&old)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let fresh = root.path().join("asset-store-pitch-batch-4294967295-1");
        fs::create_dir(&fresh).unwrap();
        let live = root
            .path()
            .join(format!("jpdb-live-acceptance-{}-17", std::process::id()));
        fs::create_dir(&live).unwrap();
        File::open(&live)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let foreign = root.path().join("foreign-app-4294967295-1");
        fs::create_dir(&foreign).unwrap();
        let link = root.path().join("yarxi-live-acceptance-4294967295-1");
        symlink(target.path(), &link).unwrap();
        let inner_link = root.path().join("kanji-0123456789abcdef0123456789abcdef");
        fs::create_dir(&inner_link).unwrap();
        symlink(target.path(), inner_link.join("escape")).unwrap();
        File::open(&inner_link)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(172800)),
            )
            .unwrap();
        let report = cleanup_legacy_under(root.path(), DEFAULT_ORPHAN_MIN_AGE).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.removed_bytes, 11);
        assert_eq!(report.skipped, 4);
        assert!(!old.exists());
        assert!(target.path().join("keep").exists());
        for path in [&fresh, &live, &foreign, &link, &inner_link] {
            assert!(path.exists());
        }
        assert_eq!(
            cleanup_legacy_under(root.path(), DEFAULT_ORPHAN_MIN_AGE)
                .unwrap()
                .removed,
            0
        );
    }

    #[test]
    fn marker_symlink_and_writable_entries_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let root = sandbox();
        let target = sandbox();
        let marker_link = make_orphan(root.path(), Duration::from_secs(172800));
        let marker_bytes = fs::read(marker_link.join(MARKER)).unwrap();
        let outside = target.path().join("marker");
        fs::write(&outside, &marker_bytes).unwrap();
        fs::remove_file(marker_link.join(MARKER)).unwrap();
        symlink(&outside, marker_link.join(MARKER)).unwrap();
        let writable = make_orphan(root.path(), Duration::from_secs(172800));
        fs::write(writable.join("group-writable"), b"preserve").unwrap();
        fs::set_permissions(
            writable.join("group-writable"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert_eq!(
            cleanup_under(root.path(), Duration::ZERO).unwrap().removed,
            0
        );
        assert_eq!(fs::read(&outside).unwrap(), marker_bytes);
        assert!(writable.join("group-writable").exists());
        let before = snapshot_under(root.path()).unwrap();
        assert_eq!(before.directories, 2);
        assert!(before.bytes >= 8);
    }

    #[test]
    fn snapshot_tolerates_concurrently_removed_workspaces() {
        let root = sandbox();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..64 {
                    let owner =
                        TempWorkspace::create_under(root.path(), "parallel-snapshot").unwrap();
                    fs::create_dir(owner.path().join("nested")).unwrap();
                    fs::write(owner.path().join("nested/data"), b"payload").unwrap();
                    std::thread::yield_now();
                    owner.close().unwrap();
                }
            });
            for _ in 0..64 {
                snapshot_under(root.path()).unwrap();
                std::thread::yield_now();
            }
        });
        assert_eq!(snapshot_under(root.path()).unwrap().directories, 0);
    }
}
