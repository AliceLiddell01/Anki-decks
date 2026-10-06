use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir, Mode, OFlags, StatxFlags, open, openat, statat, statx};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TmpInventory {
    pub root: PathBuf,
    pub allocated_bytes: u64,
    pub top_level_entries: usize,
    pub mount_boundaries: usize,
    pub errors: usize,
    pub complete: bool,
    pub largest: Vec<TmpEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TmpEntry {
    pub path: PathBuf,
    pub kind: &'static str,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub modified_unix_seconds: Option<u64>,
    pub age_seconds: Option<u64>,
    pub owner_uid: Option<u32>,
    pub ownership: &'static str,
    pub live: Option<bool>,
    pub action: &'static str,
    pub reason: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OwnershipEvidence {
    pub path: PathBuf,
    pub ownership: &'static str,
    pub live: Option<bool>,
    pub action: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub mount_boundaries: usize,
    pub errors: Vec<String>,
    pub device: u64,
    pub inode: u64,
}

#[derive(Default)]
struct Totals {
    allocated: u64,
    apparent: u64,
    uids: BTreeSet<u32>,
    errors: Vec<String>,
    mount_boundaries: usize,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct InodeKey {
    device: u64,
    inode: u64,
}

/// Инвентаризирует весь `root`, не переходя через symlink и mount boundary.
/// Выделенные блоки считаются один раз на `(device, inode)`, поэтому sparse
/// files и hard links отражаются так, как они занимают место на диске.
pub fn scan_tmp(
    root: &Path,
    top_n: usize,
    evidence: &[OwnershipEvidence],
) -> io::Result<TmpInventory> {
    let root_fd = open_directory(root)?;
    let mut names = read_names(&root_fd)?;
    names.sort();
    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(names.len());
    let mut total_allocated = 0_u64;
    let mut mount_boundaries = 0_usize;
    let mut error_count = 0_usize;

    for name in names {
        let path = root.join(&name);
        let mut totals = Totals::default();
        let top_stat = match statat(&root_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => {
                let row = error_entry(&path, error.into());
                error_count += 1;
                entries.push(row);
                continue;
            }
        };
        totals.uids.insert(top_stat.st_uid);
        account_stat(
            top_stat.st_dev,
            top_stat.st_ino,
            u64::try_from(top_stat.st_blocks).unwrap_or(0),
            &mut seen,
            &mut totals,
        );
        totals.apparent = apparent_size(top_stat.st_size);
        let kind = file_kind(top_stat.st_mode);

        if kind == "directory" {
            match openat(
                &root_fd,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    let directory = File::from(fd);
                    let opened = directory.metadata()?;
                    if opened.dev() != top_stat.st_dev || opened.ino() != top_stat.st_ino {
                        totals
                            .errors
                            .push("top-level entry заменён во время scan".into());
                    } else if same_mount(&root_fd, &directory).is_err() {
                        totals.mount_boundaries += 1;
                    } else {
                        scan_directory(&directory, &mut seen, &mut totals)?;
                    }
                }
                Err(error) => totals.errors.push(error.to_string()),
            }
        }

        let modified = u64::try_from(top_stat.st_mtime).ok();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|value| value.as_secs());
        let age = modified
            .zip(now)
            .map(|(modified, now)| now.saturating_sub(modified));
        let owner_uid = if totals.uids.len() == 1 {
            totals.uids.first().copied()
        } else {
            None
        };
        let matched = evidence.iter().find(|item| item.path == path);
        let (ownership, live, action, reason) = match matched {
            Some(item) => (item.ownership, item.live, item.action, item.reason.clone()),
            None if owner_uid.is_some_and(|uid| uid != current_uid()) => (
                "foreign",
                None,
                "foreign",
                "объект принадлежит другому UID и не удаляется проектом".into(),
            ),
            None => (
                "unknown",
                None,
                "keep",
                "нет подтверждения project ownership; только инвентаризация".into(),
            ),
        };
        let entry_errors = if totals.errors.is_empty() && totals.mount_boundaries == 0 {
            None
        } else {
            error_count += totals.errors.len() + usize::from(totals.mount_boundaries > 0);
            let mut reasons = totals.errors;
            if totals.mount_boundaries > 0 {
                reasons.push(format!(
                    "пропущено вложенных mount boundary: {}",
                    totals.mount_boundaries
                ));
            }
            Some(reasons.join("; "))
        };
        mount_boundaries += totals.mount_boundaries;
        total_allocated = total_allocated.saturating_add(totals.allocated);
        entries.push(TmpEntry {
            path,
            kind,
            allocated_bytes: totals.allocated,
            apparent_bytes: totals.apparent,
            modified_unix_seconds: modified,
            age_seconds: age,
            owner_uid,
            ownership,
            live,
            action,
            reason,
            error: entry_errors,
        });
    }

    entries.sort_by(|left, right| {
        right
            .allocated_bytes
            .cmp(&left.allocated_bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    let top_level_entries = entries.len();
    entries.truncate(top_n);
    Ok(TmpInventory {
        root: root.to_path_buf(),
        allocated_bytes: total_allocated,
        top_level_entries,
        mount_boundaries,
        errors: error_count,
        complete: error_count == 0 && mount_boundaries == 0,
        largest: entries,
    })
}

/// Измеряет одно дерево без перехода через symlink или mount boundary.
pub fn measure_path(path: &Path) -> io::Result<Measurement> {
    let directory = open_directory(path)?;
    let metadata = directory.metadata()?;
    let mut totals = Totals::default();
    let mut seen = HashSet::new();
    account_stat(
        metadata.dev(),
        metadata.ino(),
        metadata.blocks(),
        &mut seen,
        &mut totals,
    );
    scan_directory(&directory, &mut seen, &mut totals)?;
    Ok(Measurement {
        path: path.to_path_buf(),
        allocated_bytes: totals.allocated,
        apparent_bytes: totals.apparent,
        mount_boundaries: totals.mount_boundaries,
        errors: totals.errors,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn scan_directory(
    directory: &File,
    seen: &mut HashSet<InodeKey>,
    totals: &mut Totals,
) -> io::Result<()> {
    let mut names = match read_names(directory) {
        Ok(names) => names,
        Err(error) => {
            totals.errors.push(error.to_string());
            return Ok(());
        }
    };
    names.sort();
    for name in names {
        let stat = match statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(error) if error == rustix::io::Errno::NOENT => continue,
            Err(error) => {
                totals.errors.push(error.to_string());
                continue;
            }
        };
        totals.uids.insert(stat.st_uid);
        account_stat(
            stat.st_dev,
            stat.st_ino,
            u64::try_from(stat.st_blocks).unwrap_or(0),
            seen,
            totals,
        );
        totals.apparent = totals.apparent.saturating_add(apparent_size(stat.st_size));
        if file_kind(stat.st_mode) != "directory" {
            continue;
        }
        let child = match openat(
            directory,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(error) => {
                totals.errors.push(error.to_string());
                continue;
            }
        };
        let after = child.metadata()?;
        if after.dev() != stat.st_dev || after.ino() != stat.st_ino {
            totals.errors.push("каталог заменён во время scan".into());
            continue;
        }
        if same_mount(directory, &child).is_err() {
            totals.mount_boundaries += 1;
            continue;
        }
        scan_directory(&child, seen, totals)?;
    }
    Ok(())
}

fn account_stat(
    device: u64,
    inode: u64,
    blocks: u64,
    seen: &mut HashSet<InodeKey>,
    totals: &mut Totals,
) {
    if seen.insert(InodeKey { device, inode }) {
        totals.allocated = totals.allocated.saturating_add(blocks.saturating_mul(512));
    }
}

fn apparent_size(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

fn open_directory(path: &Path) -> io::Result<File> {
    let absolute = path.is_absolute();
    let mut directory = File::from(open(
        if absolute { "/" } else { "." },
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => (),
            std::path::Component::Normal(name) => {
                directory = File::from(openat(
                    directory.as_fd(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
            }
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "путь содержит запрещённый компонент",
                ));
            }
        }
    }
    Ok(directory)
}

fn read_names(directory: &File) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(bytes).to_owned());
        }
    }
    Ok(names)
}

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

fn file_kind(mode: u32) -> &'static str {
    match rustix::fs::FileType::from_raw_mode(mode) {
        rustix::fs::FileType::Directory => "directory",
        rustix::fs::FileType::RegularFile => "file",
        rustix::fs::FileType::Symlink => "symlink",
        rustix::fs::FileType::Fifo => "fifo",
        rustix::fs::FileType::Socket => "socket",
        rustix::fs::FileType::CharacterDevice | rustix::fs::FileType::BlockDevice => "device",
        _ => "other",
    }
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .unwrap_or(u32::MAX)
}

fn error_entry(path: &Path, error: io::Error) -> TmpEntry {
    TmpEntry {
        path: path.to_path_buf(),
        kind: "unknown",
        allocated_bytes: 0,
        apparent_bytes: 0,
        modified_unix_seconds: None,
        age_seconds: None,
        owner_uid: None,
        ownership: "unknown",
        live: None,
        action: "error",
        reason: "не удалось безопасно просканировать объект".into(),
        error: Some(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    fn sandbox() -> (TempWorkspace, PathBuf) {
        let owner = TempWorkspace::create("repository-maintenance-scan-test")
            .expect("project-owned temp workspace создаётся");
        let path = owner.path().join("inventory");
        fs::create_dir(&path).unwrap();
        (owner, path)
    }

    #[test]
    fn scan_counts_allocated_blocks_deduplicates_hardlinks_and_skips_symlinks() {
        let (_owner, root) = sandbox();
        let payload = root.join("payload");
        fs::create_dir(&payload).unwrap();
        let file = payload.join("data.bin");
        let data = vec![0x5a; 8192];
        fs::write(&file, data).unwrap();
        fs::hard_link(&file, root.join("hardlink")).unwrap();
        symlink(&root, root.join("directory-link")).unwrap();
        symlink(&file, root.join("file-link")).unwrap();
        let sparse = root.join("sparse.bin");
        let sparse_file = fs::File::create(&sparse).unwrap();
        sparse_file.set_len(8 * 1024 * 1024).unwrap();

        let inventory = scan_tmp(&root, 20, &[]).unwrap();
        let rows = inventory
            .largest
            .iter()
            .map(|entry| (entry.path.file_name().unwrap().to_string_lossy(), entry))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(inventory.top_level_entries, 5);
        assert_eq!(rows["directory-link"].kind, "symlink");
        assert_eq!(rows["file-link"].kind, "symlink");
        assert!(rows["sparse.bin"].apparent_bytes >= 8 * 1024 * 1024);
        assert!(rows["sparse.bin"].allocated_bytes < rows["sparse.bin"].apparent_bytes);
        let file_allocated = fs::metadata(&file).unwrap().blocks() * 512;
        assert!(inventory.allocated_bytes >= file_allocated);
        assert!(inventory.allocated_bytes < file_allocated.saturating_mul(3) + 4096);
    }

    #[test]
    fn inventory_is_bounded_and_applies_ownership_only_from_evidence() {
        let (_owner, root) = sandbox();
        fs::write(root.join("foreign-entry"), b"foreign").unwrap();
        fs::write(root.join("owned-entry"), b"owned").unwrap();
        let evidence = [OwnershipEvidence {
            path: root.join("owned-entry"),
            ownership: "project-owned",
            live: Some(false),
            action: "delete",
            reason: "подтверждённый orphan".into(),
        }];
        let inventory = scan_tmp(&root, 1, &evidence).unwrap();
        assert_eq!(inventory.top_level_entries, 2);
        assert_eq!(inventory.largest.len(), 1);
        let full = scan_tmp(&root, 20, &evidence).unwrap();
        assert!(full.largest.iter().any(|entry| {
            entry.path.ends_with("owned-entry") && entry.ownership == "project-owned"
        }));
        assert!(full.largest.iter().any(|entry| {
            entry.path.ends_with("foreign-entry") && entry.ownership == "unknown"
        }));
    }
}
