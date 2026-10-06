use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use rustix::fs::{FlockOperation, flock};

pub struct MaintenanceLock {
    _file: File,
}

impl MaintenanceLock {
    pub fn acquire() -> io::Result<Self> {
        let directory = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("anki-decks-maintenance");
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "каталог lock не является обычным каталогом",
                ));
            }
            Ok(metadata) => {
                let uid = fs::metadata("/proc/self")?.uid();
                if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "каталог lock принадлежит другому UID или доступен группе/остальным",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&directory)?;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) => return Err(error),
        }
        let path = directory.join("maintenance.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let metadata = file.metadata()?;
        let uid = fs::metadata("/proc/self")?.uid();
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "lock file принадлежит другому UID или доступен группе/остальным",
            ));
        }
        flock(&file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self { _file: file })
    }
}
