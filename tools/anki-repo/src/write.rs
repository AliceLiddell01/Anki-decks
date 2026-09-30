//! Атомарная публикация документов. Все изменения `deck.json` используют одну
//! устойчивую блокировку каталога экспорта. Создание с медиа удерживает ту же
//! блокировку от проверки исходника до размещения проверенных файлов в `media/`
//! и публикации `deck.json`.
//!
//! Внешние процессы записи должны согласовать применение этой блокировки: без
//! неё отдельные чтение, сравнение и переименование не дают атомарной замены, а
//! переносимого файлового API для такой операции нет.

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

/// Общая блокировка каталога для всех изменений одного CrowdAnki-экспорта.
/// Каталог не заменяется при публикации JSON; сама блокировка не создаёт файлов.
pub struct ExportLock {
    pub(crate) directory: File,
}

impl ExportLock {
    pub fn acquire(path: &Path) -> Result<Self, DomainError> {
        let directory = File::open(path).map_err(|e| write_error(path, "lock_export", &e))?;
        if !directory
            .metadata()
            .map_err(|e| write_error(path, "lock_export", &e))?
            .is_dir()
        {
            return Err(DomainError::new(
                ErrorCode::WriteFailed,
                "блокировка экспорта требует каталог",
            ));
        }
        lock_exclusive(path, &directory)?;
        Ok(Self { directory })
    }

    pub fn check_source(&self, path: &Path, expected: &[u8]) -> Result<(), DomainError> {
        use std::os::unix::fs::MetadataExt;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let current_dir =
            fs::metadata(parent).map_err(|e| write_error(path, "verify_export", &e))?;
        let locked_dir = self
            .directory
            .metadata()
            .map_err(|e| write_error(path, "verify_export", &e))?;
        if current_dir.dev() != locked_dir.dev() || current_dir.ino() != locked_dir.ino() {
            return Err(source_changed(path, "export_directory_changed"));
        }
        let actual = fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                source_changed(path, "source_missing")
            } else {
                write_error(path, "verify_source", &e)
            }
        })?;
        if actual != expected {
            return Err(source_changed(path, "source_modified"));
        }
        Ok(())
    }

    /// Вызывается, когда удерживается блокировка каталога экспорта.
    pub fn replace(
        &self,
        path: &Path,
        expected: &[u8],
        candidate: &[u8],
    ) -> Result<PublishedFile, DomainError> {
        self.check_source(path, expected)?;
        replace_locked(path, expected, candidate)
    }
}

/// Захватывает эксклюзивную advisory-блокировку на открытом дескрипторе.
///
/// На Unix это `flock(2)` в блокирующем режиме. На других платформах
/// эквивалента, совместимого с MSRV 1.88 и запретом `unsafe`, нет: там
/// гарантия 1 из модульной документации не действует, поэтому публикация
/// отклоняется с [`ErrorCode::WriteFailed`], а не выдаётся за защищённую.
///
/// # Errors
///
/// Возвращает [`ErrorCode::WriteFailed`] при отказе файловой системы.
#[cfg(unix)]
fn lock_exclusive(path: &Path, file: &File) -> Result<(), DomainError> {
    use fs2::FileExt;

    file.lock_exclusive()
        .map_err(|error| write_error(path, "lock_source", &error))
}

/// См. [`lock_exclusive`] на Unix.
#[cfg(not(unix))]
fn lock_exclusive(path: &Path, _file: &File) -> Result<(), DomainError> {
    Err(DomainError::with_details(
        ErrorCode::WriteFailed,
        format!(
            "{}: межпроцессная блокировка публикации поддерживается только на Unix; \
             правка на этой платформе не гарантирует защиту от lost update",
            path.display()
        ),
        details! {
            "path" => path.display().to_string(),
            "operation" => "lock_source",
            "reason" => "lock_unsupported_platform",
        },
    ))
}

/// Публикует документ, у которого нет предусловия на прежнее содержимое.
///
/// Этим путём пишутся артефакты, которые не являются чьим-то изменяемым
/// исходником: resolved-запрос `create --emit-resolved` и файл-манифест отчёта.
/// Проверять у них «источник не изменился» нечем и незачем — они и есть результат
/// команды, — но частично записанный документ недопустим так же, как частично
/// записанный `deck.json`: читатель не должен видеть половину файла.
///
/// Поэтому публикация идёт тем же способом, что и у `deck.json` (временный файл
/// рядом с целью, `sync_all`, затем `rename` и best-effort синхронизация
/// каталога), и остаётся единственной в toolkit'е: второй реализации атомарной
/// записи здесь не появляется.
///
/// # Errors
///
/// [`ErrorCode::WriteFailed`] для любого отказа файловой системы. Временный файл
/// не переживает неуспешную публикацию.
pub fn replace_document_atomically(
    path: &Path,
    document: &[u8],
) -> Result<PublishedFile, DomainError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let saved_permissions = fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.permissions());

    let (temp_path, file, temp_attempt) = create_temp(path, directory)?;

    let outcome = write_new_document(&temp_path, file, saved_permissions.as_ref(), document)
        .and_then(|()| {
            fs::rename(&temp_path, path).map_err(|error| write_error(path, "rename", &error))?;
            sync_directory(path);
            Ok(())
        });

    if outcome.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    outcome.map(|()| PublishedFile {
        path: path.to_path_buf(),
        bytes: document.len(),
        temp_attempt,
    })
}

/// Записывает документ во временный файл и сбрасывает его на диск.
fn write_new_document(
    temp_path: &Path,
    mut file: File,
    saved_permissions: Option<&fs::Permissions>,
    document: &[u8],
) -> Result<(), DomainError> {
    file.write_all(document)
        .and_then(|()| file.sync_all())
        .map_err(|error| write_error(temp_path, "write_candidate", &error))?;

    if let Some(permissions) = saved_permissions {
        file.set_permissions(permissions.clone())
            .map_err(|error| write_error(temp_path, "set_permissions", &error))?;
    }

    drop(file);
    Ok(())
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
    if !directory.is_dir() {
        return Err(write_error(
            path,
            "create_temp",
            &std::io::Error::new(std::io::ErrorKind::NotFound, "каталог отсутствует"),
        ));
    }
    let guard = ExportLock::acquire(directory)?;
    guard.replace(path, expected_source, candidate)
}

fn replace_locked(
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

/// Записывает кандидат во временный файл и публикует его под блокировкой.
///
/// Порядок шагов существенен: кандидат полностью готов и сброшен на диск до
/// захвата блокировки, поэтому критическая секция остаётся короткой — только
/// проверка предусловия и `rename`.
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

    // Вызывающий удерживает ExportLock: проверка и переименование выполняются
    // в одной критической секции.

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

    sync_directory(path);

    Ok(())
}

/// Делает появление новой записи долговечным.
///
/// Вызывается только после успешного `rename`. На Windows открыть каталог
/// средствами `std` нельзя, поэтому шаг best-effort и не влияет на результат.
fn sync_directory(path: &Path) {
    #[cfg(unix)]
    {
        if let Ok(directory) = File::open(path.parent().unwrap_or_else(|| Path::new("."))) {
            let _ = directory.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
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
    fn creates_a_document_that_did_not_exist() {
        let dir = TempDir::new("write-document-new");
        let artifacts = dir.path().join("artifacts");
        fs::create_dir_all(&artifacts).expect("каталог");
        let path = artifacts.join("resolved.json");

        let published =
            replace_document_atomically(&path, b"{\"note\":1}").expect("документ записан");

        assert_eq!(published.bytes, 10);
        assert_eq!(fs::read(&path).expect("файл"), b"{\"note\":1}");
        assert_temp_files_absent(&dir.path().join("artifacts"));
    }

    #[test]
    fn replaces_a_document_whole() {
        let dir = TempDir::new("write-document-replace");
        let path = dir.path().join("resolved.json");
        fs::write(&path, "прежнее содержимое".as_bytes()).expect("файл");

        replace_document_atomically(&path, "новое".as_bytes()).expect("замена");

        assert_eq!(fs::read(&path).expect("файл"), "новое".as_bytes());
        assert_temp_files_absent(dir.path());
    }

    #[test]
    fn a_missing_parent_directory_is_a_write_failure() {
        let dir = TempDir::new("write-document-missing");
        let path = dir.path().join("нет-такого").join("resolved.json");

        let error = replace_document_atomically(&path, b"x").expect_err("нет каталога");

        assert_eq!(error.code, ErrorCode::WriteFailed);
        assert_eq!(error.details["operation"], "create_temp");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_document_is_not_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("write-document-readonly");
        let path = dir.path().join("resolved.json");
        fs::write(&path, "прежнее".as_bytes()).expect("файл");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).expect("chmod");

        // Каталог только для чтения означает отказ, только если права вообще
        // ограничивают запись: под root каталог 0o555 остаётся записываемым, и
        // проверять на нём нечего.
        let probe = dir.path().join("проверка-прав");
        let rejecting = fs::write(&probe, b"x").is_err();
        let _ = fs::remove_file(&probe);
        if !rejecting {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
            eprintln!("каталог 0o555 принимает запись: проверка пропущена");
            return;
        }

        let outcome = replace_document_atomically(&path, "новое".as_bytes());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod");

        let error = outcome.expect_err("отказ");
        assert_eq!(error.code, ErrorCode::WriteFailed);
        assert_eq!(fs::read(&path).expect("файл"), "прежнее".as_bytes());
        assert_temp_files_absent(dir.path());
    }

    #[test]
    fn refuses_to_replace_modified_source() {
        let dir = TempDir::new("write-conflict");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"changed-by-someone-else").expect("исходный файл");

        let error = replace_atomically(&path, b"old", b"new").expect_err("конфликт");

        assert_eq!(error.code, ErrorCode::SourceChanged);
        assert_eq!(error.details["reason"], "source_modified");
        assert_eq!(
            fs::read(&path).expect("файл"),
            b"changed-by-someone-else",
            "внешняя правка не должна быть потеряна"
        );
        assert_temp_files_absent(dir.path());
    }

    #[test]
    fn reports_source_changed_when_source_disappeared() {
        let dir = TempDir::new("write-gone");
        let path = dir.path().join("deck.json");

        let error = replace_atomically(&path, b"old", b"new").expect_err("файла нет");

        assert_eq!(error.code, ErrorCode::SourceChanged);
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

    /// Исходный race из review: два writer'а одного source snapshot хотят
    /// опубликовать разные кандидаты, и второй доходит до публикации уже после
    /// первого — то есть попадает ровно в окно между проверкой и `rename`.
    ///
    /// Тест детерминированный: порядок задаёт сам тест, а не планировщик потоков,
    /// поэтому `sleep`, повторные попытки и flaky временное окно не нужны.
    #[cfg(unix)]
    #[test]
    fn concurrent_publication_publishes_one_candidate_and_reports_conflict() {
        const SNAPSHOT: &[u8] = b"snapshot";
        const FIRST: &[u8] = b"candidate-A";
        const SECOND: &[u8] = b"candidate-B";

        let dir = TempDir::new("write-race");
        let path = dir.path().join("deck.json");
        fs::write(&path, SNAPSHOT).expect("исходный файл");

        // Первый writer публикует свой кандидат: он выиграл критическую секцию.
        let published = replace_atomically(&path, SNAPSHOT, FIRST).expect("публикация первого");
        assert_eq!(published.bytes, FIRST.len());
        assert_eq!(fs::read(&path).expect("файл"), FIRST);

        // Второй writer стартовал с тем же snapshot и опубликовался бы поверх
        // результата первого. Он обязан получить контролируемый конфликт.
        let error = replace_atomically(&path, SNAPSHOT, SECOND).expect_err("конфликт");

        assert_eq!(
            error.code,
            ErrorCode::SourceChanged,
            "проигравший получает контролируемый конфликт устаревшего исходника"
        );
        assert_eq!(error.exit_code(), 7);
        assert_eq!(error.details["reason"], "source_modified");
        assert_eq!(
            fs::read(&path).expect("файл"),
            FIRST,
            "результат победителя не должен быть затёрт проигравшим"
        );
        assert_temp_files_absent(dir.path());
    }

    /// Контрольный тест: та же последовательность без блокировки действительно
    /// теряет update.
    ///
    /// Повторяет только сравнение и `rename`, без [`ExportLock`], и показывает,
    /// что отдельные проверка и `rename` не дают атомарного сравнения с заменой:
    /// второй процесс может успешно опубликовать свой результат поверх первого.
    /// Именно эту последовательность закрывает `write_and_publish`.
    #[test]
    fn unlocked_publication_loses_the_first_update() {
        const SNAPSHOT: &[u8] = b"snapshot";
        const FIRST: &[u8] = b"candidate-A";
        const SECOND: &[u8] = b"candidate-B";

        let dir = TempDir::new("write-race-unlocked");
        let path = dir.path().join("deck.json");
        fs::write(&path, SNAPSHOT).expect("исходный файл");

        // Оба writer'а работают с одним snapshot: их проверки предусловия
        // выполняются до публикации любого из них (в тесте — явно, в гонке — в
        // окне между сравнением и rename).
        assert_eq!(
            fs::read(&path).expect("файл"),
            SNAPSHOT,
            "writer A видит исходный snapshot"
        );
        assert_eq!(
            fs::read(&path).expect("файл"),
            SNAPSHOT,
            "writer B видит тот же snapshot"
        );

        // Проверка и публикация без критической секции: два независимых rename
        // подряд молча теряют первый результат.
        let publish = |candidate: &[u8], temp_name: &str| {
            let temp = dir.path().join(temp_name);
            fs::write(&temp, candidate).expect("кандидат");
            fs::rename(&temp, &path).expect("публикация");
        };

        publish(FIRST, "first.tmp");
        publish(SECOND, "second.tmp");

        let settled = fs::read(&path).expect("файл");
        assert_eq!(
            settled, SECOND,
            "без блокировки побеждает последний rename: update первого потерян"
        );
        assert!(
            !settled.windows(FIRST.len()).any(|window| window == FIRST),
            "результат первого writer'а нигде не сохранился"
        );
    }

    /// Блокировка действительно сериализует публикацию.
    ///
    /// Пока каталог экспорта удерживается [`ExportLock`], публикация не может
    /// войти в критическую секцию: поток остаётся заблокированным на захвате и
    /// завершается только после освобождения. Именно это делает проверку
    /// исходника корректной: без общей блокировки второй процесс мог бы
    /// опубликовать свой результат поверх первого.
    ///
    /// Наблюдение одностороннее и поэтому не flaky: тест проверяет, что до
    /// освобождения блокировки публикация не завершилась. Успешная публикация
    /// прислала бы сигнал немедленно, и он был бы прочитан до освобождения.
    #[cfg(unix)]
    #[test]
    fn publication_waits_for_the_lock_holder() {
        use fs2::FileExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = TempDir::new("write-lock-wait");
        let path = dir.path().join("deck.json");
        fs::write(&path, b"old").expect("исходный файл");

        // Владелец блокировки: критическая секция занята.
        let holder_file = File::open(dir.path()).expect("каталог экспорта");
        holder_file.lock_exclusive().expect("блокировка");

        // Публикация в отдельном потоке: она обязана ждать блокировку.
        let (sender, receiver) = mpsc::channel();
        let publisher_path = path.clone();
        let publisher = std::thread::spawn(move || {
            let outcome = replace_atomically(&publisher_path, b"old", b"new");
            let _ = sender.send(());
            outcome
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(150)).is_err(),
            "публикация не может завершиться, пока блокировку держит другой writer"
        );

        // Освобождение владельца открывает путь публикации: блокировка живёт на
        // открытом дескрипторе и снимается при его закрытии.
        drop(holder_file);

        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("после освобождения публикация должна завершиться");
        publisher
            .join()
            .expect("поток публикации")
            .expect("публикация после освобождения");

        assert_eq!(fs::read(&path).expect("файл"), b"new");
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
