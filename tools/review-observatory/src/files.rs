//! Дескрипторный allowlist: никакого чтения worktree, logs, arbitrary paths или symlinks.
use crate::dto::Provenance;
use rustix::fs::{Mode, OFlags, open, openat};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SMALL_BYTES: u64 = 4 * 1024 * 1024;

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
pub fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn namespace(value: &str) -> bool {
    value == "local"
        || value
            .parse::<u64>()
            .is_ok_and(|n| n > 0 && n.to_string() == value)
}
pub fn variant(value: &str) -> bool {
    value.strip_prefix("snapshot-").is_some_and(|s| hex(s, 32))
}
pub fn opaque_id(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()))[..24].into()
}

#[derive(Debug)]
pub struct SafeRoot {
    pub path: PathBuf,
    directory: File,
}
pub struct DirectoryScan {
    pub names: Vec<String>,
    pub diagnostics: Vec<String>,
}
impl SafeRoot {
    pub fn new(path: &Path) -> Result<Self, String> {
        let path = path
            .canonicalize()
            .map_err(|_| "Корневой каталог недоступен")?;
        let directory = open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| "Корневой каталог не открыт")?;
        Ok(Self {
            path,
            directory: directory.into(),
        })
    }
    fn file(&self, relative: &Path, flags: OFlags) -> Result<File, String> {
        let components: Vec<_> = relative.components().collect();
        if components.is_empty()
            || components
                .iter()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("Недопустимый относительный путь".into());
        }
        let mut current = self
            .directory
            .try_clone()
            .map_err(|_| "Дескриптор недоступен")?;
        for (i, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                unreachable!()
            };
            let requested = if i + 1 == components.len() {
                flags
            } else {
                OFlags::RDONLY | OFlags::DIRECTORY
            };
            current = openat(
                &current,
                Path::new(name),
                requested | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| format!("Недоступный или небезопасный артефакт: {error}"))?;
        }
        Ok(current)
    }
    pub fn read(&self, relative: &Path, max: u64) -> Result<(Vec<u8>, Provenance), String> {
        let file = self.file(relative, OFlags::RDONLY)?;
        let before = file.metadata().map_err(|_| "Метаданные недоступны")?;
        if !before.is_file() || before.len() > max {
            return Err("Артефакт не является обычным файлом или превышает лимит".into());
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        (&file)
            .take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Ошибка ограниченного чтения")?;
        let after = file.metadata().map_err(|_| "Метаданные изменились")?;
        if bytes.len() as u64 > max
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(
                "Артефакт менялся во время чтения; повторная проверка будет выполнена".into(),
            );
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        Ok((
            bytes,
            Provenance {
                path: relative.to_string_lossy().into(),
                sha256: Some(digest),
                modified_unix_ms: before
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64),
            },
        ))
    }
    pub fn stamp(&self, relative: &Path) -> String {
        match self
            .file(relative, OFlags::RDONLY)
            .and_then(|f| f.metadata().map_err(|e| e.to_string()))
        {
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                format!(
                    "{}:{}:{}:{:?}",
                    m.dev(),
                    m.ino(),
                    m.len(),
                    m.modified().ok()
                )
            }
            Err(error) => error,
        }
    }
    pub fn lock_active(&self, relative: &Path) -> bool {
        let Ok(file) = self.file(relative, OFlags::RDONLY) else {
            return false;
        };
        use fs2::FileExt;
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                false
            }
            Err(_) => true,
        }
    }
    pub fn length(&self, relative: &Path) -> Result<u64, String> {
        let file = self.file(relative, OFlags::RDONLY)?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("Ожидался обычный артефакт".into());
        }
        Ok(metadata.len())
    }
    pub fn children_checked(&self, relative: &Path) -> Result<DirectoryScan, String> {
        let directory = match self.file(relative, OFlags::RDONLY | OFlags::DIRECTORY) {
            Ok(d) => d,
            Err(error) => {
                if fs::symlink_metadata(self.path.join(relative))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    return Ok(DirectoryScan {
                        names: Vec::new(),
                        diagnostics: Vec::new(),
                    });
                }
                return Err(format!("{}: {error}", relative.display()));
            }
        };
        let entries = rustix::fs::Dir::read_from(&directory)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
        let mut names = Vec::new();
        let mut diagnostics = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", relative.display()))?;
            if entry.file_type() == rustix::fs::FileType::Symlink {
                diagnostics.push(format!(
                    "{}: symlink в области наблюдения запрещён",
                    relative.display()
                ));
                continue;
            }
            if entry.file_type() == rustix::fs::FileType::Directory
                && let Ok(name) = entry.file_name().to_str()
                && identifier(name)
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(DirectoryScan { names, diagnostics })
    }
    pub fn database_path(&self) -> Result<PathBuf, String> {
        let relative = Path::new(".anki-repo/learning/state.sqlite");
        let file = self.file(relative, OFlags::RDONLY)?;
        if !file
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= 2 * 1024 * 1024 * 1024)
        {
            return Err("База слишком велика или не является обычным файлом".into());
        }
        // Проверены все компоненты; SQLite самостоятельно открывает этот фиксированный путь.
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = self
                .path
                .join(format!(".anki-repo/learning/state.sqlite{suffix}"));
            if fs::symlink_metadata(sidecar).is_ok_and(|m| !m.is_file()) {
                return Err("Небезопасный SQLite sidecar".into());
            }
        }
        Ok(self.path.join(relative))
    }
}
