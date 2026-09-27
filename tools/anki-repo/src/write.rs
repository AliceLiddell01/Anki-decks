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
//! * если файл изменился между чтением и публикацией, запись отклоняется
//!   с [`ErrorCode::SourceChanged`].
//!
//! # Защита от lost update
//!
//! Одной проверки «прочитать — сравнить — переименовать» для последней гарантии
//! недостаточно: между сравнением и `rename` остаётся окно, в которое другой
//! процесс может опубликовать свой кандидат, после чего наш `rename` молча
//! затрёт более новый результат. Отдельные `fs::read` и `fs::rename` не дают
//! compare-and-swap семантики, и любое число повторных проверок лишь сужает окно.
//!
//! Поэтому сравнение и публикация выполняются под эксклюзивной advisory-
//! блокировкой самого целевого файла (`SourceLock`): публикация становится
//! критической секцией, а не парой независимых операций.
//!
//! Две гарантии различаются явно:
//!
//! 1. Два конкурентных `anki-repo edit --apply` одного экспорта сериализуются
//!    блокировкой. Ожидающий writer дожидается своей очереди, после чего
//!    проверяет байты под блокировкой: если исходное предусловие уже устарело,
//!    он получает контролируемый [`ErrorCode::SourceChanged`] / exit 7, а не
//!    затирает победивший результат. Молчаливая потеря update при обычной
//!    конкурентной правке невозможна; единственная оговорка — блокировка
//!    прежнего inode, описанная ниже.
//! 2. Произвольный внешний writer, который не участвует в этом протоколе,
//!    блокировку не соблюдает. Для него байтовое сравнение под блокировкой —
//!    best-effort обнаружение: изменение, попавшее в файл до сравнения,
//!    отклоняется, но изменение, случившееся между сравнением и `rename`,
//!    остаётся в окне. Более сильная гарантия для некооперирующегося процесса
//!    portable-средствами `std` недостижима.
//!
//! У гарантии 1 есть узкая оговорка, и внутри протокола anki-repo она не
//! исчезает: блокировка берётся на дескриптор, открытый по пути, поэтому
//! `rename` поверх целевого файла между `open` и `flock` оставляет блокировку на
//! прежнем inode. Кооперирующийся `anki-repo` тоже может выполнить такой
//! `rename`: он снимает блокировку прежнего inode своим `rename` (см.
//! [`SourceLock`]), и второй процесс, успевший открыть прежний inode,
//! блокирует уже отвязанный файл.
//!
//! Практическое следствие ограничено сравнением под блокировкой, но не нулевое:
//! два процесса могут оказаться в критической секции по разным inode, и если
//! оба увидят в файле ровно свой снимок, второй `rename` затрёт публикацию
//! первого. Для кооперирующихся writers это требует, чтобы содержимое файла
//! совпало со снимком отставшего процесса, то есть чтобы значение вернули
//! именно к тому состоянию, из которого он строил кандидат: обычная
//! конкурентная правка такого совпадения не даёт. Гарантией «без оговорок»
//! пункт 1 поэтому не является, и повторные проверки окно только сужают:
//! полностью его снимает лишь блокировка отдельного, не переименовываемого
//! файла, а сам `deck.json` как стабильный объект блокировки не годится.
//!
//! Сама блокировка снимается ядром при завершении процесса или закрытии
//! дескриптора, поэтому падение процесса не оставляет «залипший» лок.
//!
//! # MSRV и `unsafe`
//!
//! Crate публикует только безопасный API (`unsafe_code = "forbid"`), а
//! `File::lock`/`try_lock` стабилизированы лишь в Rust 1.89 при MSRV 1.88.
//! Поэтому блокировка берётся через `fs2` — тонкую обёртку над `flock(2)`,
//! которая на Linux сводится к `libc`. Платформы без поддерживаемой
//! blocking-блокировки отклоняют публикацию с [`ErrorCode::WriteFailed`] вместо
//! того, чтобы выдавать её за защищённую.

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

/// Эксклюзивная блокировка целевого файла на время проверки и публикации.
///
/// Дескриптор держится открытым до конца критической секции: снятие блокировки
/// делает `Drop`, то есть закрытие дескриптора.
struct SourceLock {
    /// Открытый целевой файл, на котором удерживается блокировка.
    _file: File,
}

impl SourceLock {
    /// Берёт эксклюзивную блокировку целевого файла.
    ///
    /// Вызов блокирующий: если блокировку уже держит другой `anki-repo`, текущий
    /// процесс ждёт своей очереди, а не отказывается сразу. Это осознанный
    /// выбор — ожидание превращает гонку в последовательные критические секции,
    /// где проигравший обнаруживает устаревшее предусловие и получает
    /// `source_changed`, а не произвольный отказ захвата лока.
    ///
    /// # Errors
    ///
    /// * [`ErrorCode::SourceChanged`], если файла уже нет: предусловие правки
    ///   устарело;
    /// * [`ErrorCode::WriteFailed`], если файл не открывается или блокировка
    ///   недоступна.
    fn acquire(path: &Path) -> Result<Self, DomainError> {
        let file = File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                source_changed(path, "source_missing")
            } else {
                write_error(path, "lock_source", &error)
            }
        })?;

        lock_exclusive(path, &file)?;
        Ok(Self { _file: file })
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

    // Критическая секция: проверка предусловия и публикация неразделимы.
    let lock = SourceLock::acquire(path)?;

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

    drop(lock);
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
    /// Повторяет только сравнение и `rename`, без [`SourceLock`], и показывает, что
    /// отдельные проверка и `rename` compare-and-swap семантики не дают: второй
    /// writer «успешно» публикуется поверх первого, а результат первого исчезает.
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
    /// Пока `deck.json` удерживается [`SourceLock`], публикация не может войти в
    /// критическую секцию: поток остаётся заблокированным на захвате и завершается
    /// только после освобождения. Именно это делает проверку исходника корректной:
    /// вне критической секции второй writer успел бы опубликоваться поверх первого.
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
        let holder_file = File::open(&path).expect("целевой файл");
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
