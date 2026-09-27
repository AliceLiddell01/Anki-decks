//! Атомарная замена `deck.json` при точечной правке.
//!
//! Здесь нет доменной логики правки: модуль только безопасно публикует уже
//! полностью проверенные байты кандидата. Гарантии:
//!
//! * читатель никогда не видит частично записанный `deck.json`;
//! * исходный файл не теряется: при любом отказе до `rename` он остаётся на
//!   месте, а временный файл удаляется;
//! * права доступа сохраняются (`rename` переносит права временного файла,
//!   поэтому права копируются заранее);
//! * если файл изменился между чтением и публикацией, запись отклоняется.
//!
//! Публикация выполняется `std`-средствами: `unsafe` в crate запрещён, а
//! `File::lock`/`try_lock` появились в 1.89 и несовместимы с MSRV 1.88,
//! поэтому конфликт обнаруживается сравнением байтов перед `rename`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::details;
use crate::error::{DomainError, ErrorCode};

/// Сколько раз пробовать создать временный файл с уникальным именем.
const TEMP_ATTEMPTS: u32 = 8;

/// Результат успешной публикации кандидата.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFile {
    /// Итоговый путь.
    pub path: PathBuf,
    /// Сколько байтов записано.
    pub bytes: usize,
    /// Номер попытки создания временного файла, с которой получилось (с нуля).
    pub temp_attempt: u32,
}

/// Публикует `candidate` вместо `path`, ожидая там байты `expected_source`.
///
/// # Errors
///
/// * [`ErrorCode::SourceChanged`], если `path` больше не содержит
///   `expected_source` (файл изменили между чтением и записью); сам файл при
///   этом не трогается;
/// * [`ErrorCode::WriteFailed`] для любого отказа файловой системы.
pub fn replace_atomically(
    path: &Path,
    expected_source: &[u8],
    candidate: &[u8],
) -> Result<PublishedFile, DomainError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let saved_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());

    let (temp_path, file, temp_attempt) = create_temp(path, directory)?;

    let outcome = write_and_publish(
        path,
        &temp_path,
        file,
        saved_permissions.as_ref(),
        expected_source,
        candidate,
    );

    if outcome.is_err() {
        // Временный файл не должен переживать неуспешную публикацию. Ошибка
        // удаления вторична: основная причина отказа важнее.
        let _ = fs::remove_file(&temp_path);
    }

    outcome.map(|()| PublishedFile {
        path: path.to_path_buf(),
        bytes: candidate.len(),
        temp_attempt,
    })
}

/// Создаёт временный файл рядом с целевым.
///
/// `create_new` не даёт перезаписать чужой файл, поэтому гонка на имя
/// разрешается повтором с другим суффиксом, а не затиранием.
fn create_temp(path: &Path, directory: &Path) -> Result<(PathBuf, File, u32), DomainError> {
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "deck.json".to_string());
    let pid = std::process::id();

    let mut last_error: Option<std::io::Error> = None;
    for attempt in 0..TEMP_ATTEMPTS {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|delta| delta.as_nanos())
            .unwrap_or(0);
        let name = format!(".{stem}.tmp-{pid}-{attempt}-{nanos}");
        let temp_path = directory.join(name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file, attempt)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(write_error(path, "create_temp", &error)),
        }
    }

    let error =
        last_error.unwrap_or_else(|| std::io::Error::other("не удалось создать временный файл"));
    Err(write_error(path, "create_temp", &error))
}

/// Записывает кандидат во временный файл и публикует его.
fn write_and_publish(
    path: &Path,
    temp_path: &Path,
    mut file: File,
    saved_permissions: Option<&fs::Permissions>,
    expected_source: &[u8],
    candidate: &[u8],
) -> Result<(), DomainError> {
    file.write_all(candidate)
        .and_then(|()| file.sync_all())
        .map_err(|error| write_error(path, "write_candidate", &error))?;

    if let Some(permissions) = saved_permissions {
        file.set_permissions(permissions.clone())
            .map_err(|error| write_error(path, "set_permissions", &error))?;
    }

    // Закрываем дескриптор до rename: содержимое уже синхронизировано.
    drop(file);

    let current = fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            source_changed(path, "source_missing")
        } else {
            write_error(path, "verify_source", &error)
        }
    })?;

    if current != expected_source {
        return Err(source_changed(path, "source_modified"));
    }

    fs::rename(temp_path, path).map_err(|error| write_error(path, "rename", &error))?;

    // Синхронизация каталога делает появление новой записи долговечным. На
    // Windows открыть каталог средствами `std` нельзя, поэтому шаг
    // best-effort и не влияет на результат.
    #[cfg(unix)]
    {
        if let Ok(directory) = File::open(path.parent().unwrap_or_else(|| Path::new("."))) {
            let _ = directory.sync_all();
        }
    }

    Ok(())
}

/// Готовит ошибку отказа записи.
fn write_error(path: &Path, operation: &str, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!(
            "не удалось атомарно заменить {} на шаге {operation}: {error}",
            path.display()
        ),
        details! {
            "path" => path.display().to_string(),
            "operation" => operation,
            "io_error" => error.to_string(),
        },
    )
}

/// Готовит ошибку конкурентного изменения файла.
fn source_changed(path: &Path, reason: &str) -> DomainError {
    DomainError::with_details(
        ErrorCode::SourceChanged,
        format!(
            "{} изменился между проверкой и записью; правка отменена, файл не тронут",
            path.display()
        ),
        details! {
            "path" => path.display().to_string(),
            "reason" => reason,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn publishes_candidate_and_keeps_no_temp_files() {
        let dir = TempDir::new("write-publish");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"old").expect("исходный файл");

        let published =
            replace_atomically(&path, b"old", b"new").expect("публикация должна пройти");

        assert_eq!(published.bytes, 3);
        assert_eq!(fs::read(&path).expect("файл"), b"new");
        assert_temp_files_absent(dir.path());
    }

    #[test]
    fn refuses_to_replace_modified_source() {
        let dir = TempDir::new("write-conflict");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"changed-by-someone-else").expect("исходный файл");

        let error = replace_atomically(&path, b"old", b"new").expect_err("конфликт");

        assert_eq!(error.code, ErrorCode::SourceChanged);
        assert_eq!(
            fs::read(&path).expect("файл"),
            b"changed-by-someone-else",
            "внешняя правка не должна быть потеряна"
        );
        assert_temp_files_absent(dir.path());
    }

    #[test]
    fn reports_write_failure_for_missing_directory() {
        let dir = TempDir::new("write-missing");
        let path = dir.path().join("нет-такого").join("deck.json");

        let error = replace_atomically(&path, b"old", b"new").expect_err("нет каталога");

        assert_eq!(error.code, ErrorCode::WriteFailed);
        assert_eq!(error.details["operation"], "create_temp");
    }

    #[cfg(unix)]
    #[test]
    fn preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("write-mode");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"old").expect("исходный файл");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");

        replace_atomically(&path, b"old", b"new").expect("публикация");

        let mode = fs::metadata(&path)
            .expect("метаданные")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "права исходного файла должны сохраниться"
        );
    }

    #[test]
    fn accepts_identical_candidate_without_changing_bytes() {
        let dir = TempDir::new("write-identical");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"same").expect("исходный файл");
        let before = fs::metadata(&path).expect("метаданные").modified().ok();

        replace_atomically(&path, b"same", b"same").expect("публикация");

        assert_eq!(fs::read(&path).expect("файл"), b"same");
        let after = fs::metadata(&path).expect("метаданные").modified().ok();
        assert!(before.is_some() && after.is_some());
        assert_temp_files_absent(dir.path());
    }

    /// Проверяет, что после публикации рядом не осталось временных файлов.
    fn assert_temp_files_absent(directory: &Path) {
        let leftovers: Vec<String> = fs::read_dir(directory)
            .expect("чтение каталога")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остались временные файлы: {leftovers:?}"
        );
    }
}
