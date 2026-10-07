use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{CWD, FlockOperation, Mode, OFlags, flock, mkdirat, open, openat};

/// Общая неблокирующая блокировка для очистки и управляемых сборок.
/// Дескриптор закрывается при `Drop` и не наследуется дочерними командами.
pub struct MaintenanceLock {
    _file: File,
}

impl MaintenanceLock {
    pub fn acquire() -> io::Result<Self> {
        let cache_home = crate::policy::cache_home().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "не задан HOME или XDG_CACHE_HOME для общей блокировки обслуживания",
            )
        })?;
        let shared_parent = cache_home.join("anki-decks");
        fs::create_dir_all(&shared_parent)?;
        let directory = shared_parent.join("maintenance-lock");
        Self::acquire_at(&directory)
    }

    /// Использует явно заданный приватный каталог блокировки, не меняя окружение.
    pub fn acquire_at(directory: &Path) -> io::Result<Self> {
        match mkdirat(CWD, directory, Mode::from_raw_mode(0o700)) {
            Ok(()) => (),
            Err(rustix::io::Errno::EXIST) => (),
            Err(error) => return Err(error.into()),
        }
        let directory_fd = File::from(open(
            directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let metadata = directory_fd.metadata()?;
        let uid = fs::metadata("/proc/self")?.uid();
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "каталог блокировки принадлежит другому UID или доступен группе/остальным",
            ));
        }
        let file = File::from(openat(
            &directory_fd,
            "maintenance.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "файл блокировки не является приватным обычным файлом текущего UID с единственной ссылкой",
            ));
        }
        flock(&file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self { _file: file })
    }
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        // Из-за `fork` другого потока то же описание открытого файла может
        // кратковременно удерживаться до `exec`, поэтому явно освобождаем `flock`,
        // не полагаясь только на закрытие файла.
        let _ = flock(&self._file, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let owner = TempWorkspace::create("maintenance-lock-test").unwrap();
        let directory = owner.path().join("locks");
        let lock = MaintenanceLock::acquire_at(&directory).unwrap();
        let error = MaintenanceLock::acquire_at(&directory).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(lock);
        assert!(MaintenanceLock::acquire_at(&directory).is_ok());
    }

    #[test]
    fn lock_rejects_symlinks_and_hardlinks() {
        let owner = TempWorkspace::create("maintenance-lock-links-test").unwrap();
        let directory = owner.path().join("locks");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = owner.path().join("outside");
        fs::write(&outside, b"foreign").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        let path = directory.join("maintenance.lock");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(MaintenanceLock::acquire_at(&directory).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(MaintenanceLock::acquire_at(&directory).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"foreign");
    }

    #[test]
    fn lock_rejects_non_private_directory() {
        let owner = TempWorkspace::create("maintenance-lock-permissions-test").unwrap();
        let directory = owner.path().join("locks");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            MaintenanceLock::acquire_at(&directory)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, 0o755);
    }
}
