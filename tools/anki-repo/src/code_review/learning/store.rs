//! Открытие локальной базы learning, pragmas, транзакции, целостность и backup.
//!
//! Режим журнала выбирается по доказанному типу файловой системы: `WAL`
//! включается только для известных локальных ФС, иначе используется честный
//! `DELETE`. Никакая операция этого модуля не выполняется при обычном сборе
//! сигналов code review: база создаётся и меняется только явными вызовами.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};

use crate::error::{DomainError, ErrorCode};

use super::model::LearningStatus;
use super::schema::{self, LEARNING_SCHEMA_VERSION};
use super::{DEFAULT_LEARNING_DATABASE, DEFAULT_LEARNING_DIRECTORY, LEARNING_POLICY_VERSION};

/// `busy_timeout` по умолчанию для явных learning-операций.
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5_000;

/// Ограниченный `busy_timeout` проверки совместимости при открытии базы.
///
/// Занятое хранилище должно сообщать о себе быстро; длительное ожидание
/// применяется только во время самих транзакций.
pub const OPEN_BUSY_TIMEOUT_MS: u64 = 250;

/// Известные локальные `f_type` файловых систем, для которых `WAL` безопасен.
///
/// Список намеренно белый: неизвестная ФС не считается локальной.
const LOCAL_FILESYSTEMS: &[(i64, &str)] = &[
    (0xEF53, "ext2/ext3/ext4"),
    (0x137D, "ext"),
    (0x58465342, "xfs"),
    (0x9123683E, "btrfs"),
    (0x01021994, "tmpfs"),
    (0x858458F6, "ramfs"),
    (0x2FC12FC1, "zfs"),
    (0xCA451A4E, "bcachefs"),
    (0x794C7630, "overlayfs"),
    (0x5346544E, "ntfs"),
    (0x0000ADF5, "affs"),
    (0x0000DEAD, "acfs"),
    (0x1BADFACE, "bfs"),
    (0x42494E4D, "binfmt"),
];

/// Известные сетевые и распределённые `f_type`, для которых `WAL` запрещён.
///
/// `fuse` включён сюда намеренно: за одним и тем же `f_type` может стоять как
/// локальный `virtiofs`, так и сетевой `sshfs`, поэтому доказанной локальности нет.
const NETWORK_FILESYSTEMS: &[(i64, &str)] = &[
    (0x6969, "nfs"),
    (0xFF534D42, "cifs"),
    (0xFE534D42, "smb2"),
    (0x517B, "smb"),
    (0x01021997, "9p"),
    (0x73757245, "coda"),
    (0x564C, "ncp"),
    (0x47504653, "gfs2"),
    (0x01161970, "ocfs2"),
    (0x5346314D, "ceph"),
    (0x65735546, "fuse/virtiofs"),
    (0x7461636F, "oracle-acfs"),
];

/// Параметры открытия локального хранилища learning.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Полный путь к файлу базы.
    pub database: PathBuf,
    /// Компонент пути для человекочитаемого вывода; абсолютный путь клона не выводится.
    pub display_path: String,
    /// Значение `busy_timeout` для явных операций.
    pub busy_timeout_ms: u64,
    /// Создавать ли базу и недостающий каталог при открытии.
    pub create: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            database: PathBuf::from(DEFAULT_LEARNING_DIRECTORY).join(DEFAULT_LEARNING_DATABASE),
            display_path: format!("{DEFAULT_LEARNING_DIRECTORY}/{DEFAULT_LEARNING_DATABASE}"),
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            create: true,
        }
    }
}

impl StoreOptions {
    /// Параметры для базы learning по умолчанию внутри корня репозитория.
    ///
    /// Абсолютный путь клона не выводится: в отчётах остаётся только
    /// репозиторный относительный путь.
    #[must_use]
    pub fn in_repository(root: &Path) -> Self {
        Self::at(
            root.join(DEFAULT_LEARNING_DIRECTORY)
                .join(DEFAULT_LEARNING_DATABASE),
        )
    }

    /// Параметры для явно заданного пути базы.
    #[must_use]
    pub fn at(database: impl Into<PathBuf>) -> Self {
        let database = database.into();
        let display_path = display_of(&database);
        Self {
            database,
            display_path,
            ..Self::default()
        }
    }
}

/// Представление пути без абсолютных компонентов пользователя.
///
/// Из абсолютного пути сохраняется только часть от каталога learning: путь
/// конкретного клона в отчёты и историю не попадает.
fn display_of(path: &Path) -> String {
    let components: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect();
    let marker: Vec<&str> = DEFAULT_LEARNING_DIRECTORY.split('/').collect();
    if marker.len() <= components.len()
        && let Some(index) = components.windows(marker.len()).position(|window| {
            window
                .iter()
                .zip(&marker)
                .all(|(value, expected)| value.to_str() == Some(*expected))
        })
    {
        return components[index..]
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_LEARNING_DATABASE.to_owned())
}

/// Решение о режиме журнала SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalModeDecision {
    /// Выбранный режим: `wal` либо `delete`.
    pub mode: String,
    /// Доказанное основание выбора.
    pub reason: String,
}

/// Локальное хранилище истории learning.
pub struct LearningStore {
    connection: Connection,
    options: StoreOptions,
    journal_mode: JournalModeDecision,
    fts5_available: bool,
}

impl std::fmt::Debug for LearningStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut structure = formatter.debug_struct("LearningStore");
        structure
            .field("database", &self.options.display_path)
            .field("journal_mode", &self.journal_mode.mode)
            .field("fts5_available", &self.fts5_available);
        if let Ok(version) = schema::read_schema_version(&self.connection) {
            structure.field("user_version", &version);
        }
        structure.finish_non_exhaustive()
    }
}

impl LearningStore {
    /// Открывает (и при необходимости создаёт) базу learning.
    ///
    /// Более новая версия схемы отвергается до любых изменений; повреждённый
    /// файл не удаляется и не перезаписывается.
    pub fn open(options: StoreOptions) -> Result<Self, DomainError> {
        let parent = options.database.parent().map(Path::to_path_buf);
        if let Some(parent) = parent.as_deref() {
            if options.create {
                std::fs::create_dir_all(parent).map_err(|error| {
                    DomainError::with_details(
                        ErrorCode::LearningStorageUnavailable,
                        format!(
                            "не удалось создать каталог локального хранилища learning: {error}"
                        ),
                        crate::details! { "database" => options.display_path.clone() },
                    )
                })?;
            } else if !parent.is_dir() {
                return Err(unavailable(
                    &options,
                    "каталог локального хранилища отсутствует",
                ));
            }
        }
        if !options.create && !options.database.is_file() {
            return Err(unavailable(&options, "файл базы learning отсутствует"));
        }
        let decision = journal_mode_decision(&options.database);
        let flags = if options.create {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
        };
        let connection =
            Connection::open_with_flags(&options.database, flags).map_err(|error| {
                map_open_error(
                    &options,
                    &error,
                    "не удалось открыть локальную базу learning",
                )
            })?;
        connection
            .busy_timeout(std::time::Duration::from_millis(OPEN_BUSY_TIMEOUT_MS))
            .map_err(|error| map_error(&error, "не удалось настроить busy_timeout"))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| map_error(&error, "не удалось включить foreign_keys"))?;
        connection
            .execute_batch(&format!("PRAGMA journal_mode = {}", decision.mode))
            .map_err(|error| map_error(&error, "не удалось выбрать режим журнала SQLite"))?;
        let fts5_available = schema::fts5_available(&connection)?;
        let mut store = Self {
            connection,
            options,
            journal_mode: decision,
            fts5_available,
        };
        let _ = schema::apply_migrations(&mut store.connection)?;
        schema::ensure_fts_index(&store.connection)?;
        store.write(|_| Ok(()))?;
        Ok(store)
    }

    /// Выполняет короткую транзакцию записи; изменение откатывается при ошибке.
    pub fn write<T>(
        &self,
        action: impl FnOnce(&LearningWrite<'_>) -> Result<T, DomainError>,
    ) -> Result<T, DomainError> {
        let connection = &self.connection;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| map_error(&error, "не удалось начать транзакцию записи"))?;
        let write = LearningWrite {
            transaction: &transaction,
            busy_timeout_ms: self.options.busy_timeout_ms,
        };
        let value = action(&write)?;
        transaction
            .commit()
            .map_err(|error| map_error(&error, "не удалось зафиксировать транзакцию записи"))?;
        Ok(value)
    }

    /// Читает историю на согласованном снимке (допускается параллельный писатель).
    pub fn read<T>(
        &self,
        action: impl FnOnce(&LearningRead<'_>) -> Result<T, DomainError>,
    ) -> Result<T, DomainError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| map_error(&error, "не удалось начать транзакцию чтения"))?;
        let read = LearningRead {
            transaction: &transaction,
        };
        let value = action(&read)?;
        transaction
            .commit()
            .map_err(|error| map_error(&error, "не удалось завершить транзакцию чтения"))?;
        Ok(value)
    }

    /// Доступ к соединению для операций обслуживания (целостность, backup).
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Параметры открытия.
    #[must_use]
    pub fn options(&self) -> &StoreOptions {
        &self.options
    }

    /// Путь к базе.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.options.database
    }

    /// Доказанное основание выбранного режима журнала.
    #[must_use]
    pub fn journal_mode_reason(&self) -> &str {
        &self.journal_mode.reason
    }

    /// Доступность FTS5 в текущей сборке SQLite.
    #[must_use]
    pub fn fts5_available(&self) -> bool {
        self.fts5_available
    }

    /// Проверяет целостность базы, ничего не меняя.
    pub fn integrity_check(&self) -> Result<(), DomainError> {
        let mut statement = self
            .connection
            .prepare("PRAGMA integrity_check")
            .map_err(|error| self.corrupt(&error))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| self.corrupt(&error))?;
        let mut failures = Vec::new();
        for row in rows {
            let row = row.map_err(|error| self.corrupt(&error))?;
            if row != "ok" {
                failures.push(row);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(self.corrupt_with_detail(format!("integrity_check: {}", failures.join("; "))))
        }
    }

    /// Создаёт корректный транзакционный снимок базы через `VACUUM INTO`.
    ///
    /// Копирование файлов `state.sqlite`, `-wal` и `-shm` не является backup и
    /// здесь не используется. Уже существующий файл назначения не перезаписывается.
    pub fn backup_to(&self, destination: &Path) -> Result<(), DomainError> {
        if destination.exists() {
            return Err(DomainError::new(
                ErrorCode::ReviewArtifactConflict,
                "Файл назначения backup уже существует; перезапись не выполняется",
            ));
        }
        if let Some(parent) = destination.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(DomainError::new(
                ErrorCode::LearningStorageUnavailable,
                "Каталог назначения backup отсутствует",
            ));
        }
        let Some(destination_text) = destination.to_str() else {
            // SQLite принимает путь только строкой UTF-8: молчаливая подмена
            // через `to_string_lossy` записала бы снимок по другому адресу.
            return Err(DomainError::with_details(
                ErrorCode::InvalidRequest,
                "Путь назначения снимка базы learning должен быть корректной строкой UTF-8: SQLite не принимает не-UTF-8 путь",
                crate::details! { "destination" => destination.display().to_string() },
            ));
        };
        self.connection()
            .execute("VACUUM INTO ?1", params![destination_text])
            .map_err(|error| map_error(&error, "не удалось создать транзакционный снимок базы"))?;
        if !destination.is_file() {
            // Фактическое назначение обязано совпасть с запрошенным: иначе об
            // этом нужно сказать, а не выдавать снимок за созданный.
            return Err(DomainError::with_details(
                ErrorCode::LearningStorageUnavailable,
                "Снимок базы learning не появился по запрошенному пути назначения",
                crate::details! { "destination" => destination.display().to_string() },
            ));
        }
        Ok(())
    }

    /// Состояние хранилища для диагностики.
    pub fn status(&self) -> Result<LearningStatus, DomainError> {
        let user_version = schema::read_schema_version(&self.connection)?;
        let generation = super::import::generation(self)?;
        Ok(LearningStatus {
            schema_version: LEARNING_SCHEMA_VERSION,
            policy_version: LEARNING_POLICY_VERSION,
            present: self.options.database.is_file(),
            database_path: self.options.display_path.clone(),
            user_version,
            journal_mode: self.journal_mode.mode.clone(),
            journal_mode_reason: self.journal_mode.reason.clone(),
            busy_timeout_ms: self.options.busy_timeout_ms,
            fts5_available: self.fts5_available,
            foreign_keys: true,
            generation,
            integrity_ok: self.integrity_check().is_ok(),
            unavailable_reason: None,
            recovery_paths: recovery_paths(&self.options.display_path),
        })
    }

    /// Доменная ошибка повреждения с путём восстановления.
    #[must_use]
    pub fn corrupt(&self, error: &rusqlite::Error) -> DomainError {
        self.corrupt_with_detail(error.to_string())
    }

    /// Доменная ошибка повреждения с произвольной деталью.
    #[must_use]
    pub fn corrupt_with_detail(&self, detail: String) -> DomainError {
        DomainError::with_details(
            ErrorCode::LearningCorrupt,
            format!(
                "Локальная база learning повреждена или не является базой SQLite: {detail}; \
                 исходные данные не удаляются — сделайте backup/export и восстановите историю"
            ),
            crate::details! {
                "database" => self.options.display_path.clone(),
                "recovery" => recovery_paths(&self.options.display_path),
            },
        )
    }
}

/// Возможные пути восстановления без удаления исходных данных.
#[must_use]
pub fn recovery_paths(database: &str) -> Vec<String> {
    vec![
        format!("code-review learning export --database {database}"),
        "code-review learning backup".to_owned(),
        "code-review learning restore --from <archive>".to_owned(),
    ]
}

fn unavailable(options: &StoreOptions, detail: &str) -> DomainError {
    DomainError::with_details(
        ErrorCode::LearningStorageUnavailable,
        format!("Локальное хранилище learning недоступно: {detail}"),
        crate::details! { "database" => options.display_path.clone() },
    )
}

/// Общая транзакция learning: только параметризованные запросы.
pub struct LearningTx<'a> {
    transaction: &'a rusqlite::Transaction<'a>,
}

impl LearningTx<'_> {
    /// Доступ к транзакции.
    #[must_use]
    pub fn transaction(&self) -> &rusqlite::Transaction<'_> {
        self.transaction
    }

    /// Чтение одной строки одним параметризованным запросом.
    pub fn query_row<T>(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
        mapper: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, DomainError> {
        self.transaction
            .query_row(sql, parameters, mapper)
            .map_err(|error| map_error(&error, "не удалось прочитать строку базы learning"))
    }
}

/// Транзакция записи: только параметризованные запросы.
pub struct LearningWrite<'a> {
    transaction: &'a rusqlite::Transaction<'a>,
    busy_timeout_ms: u64,
}

impl LearningWrite<'_> {
    /// Доступ к транзакции.
    #[must_use]
    pub fn transaction(&self) -> &rusqlite::Transaction<'_> {
        self.transaction
    }

    /// Представление той же транзакции для общих помощников чтения.
    #[must_use]
    pub fn as_tx(&self) -> LearningTx<'_> {
        LearningTx {
            transaction: self.transaction,
        }
    }

    /// Настроенный `busy_timeout` в миллисекундах.
    #[must_use]
    pub const fn busy_timeout_ms(&self) -> u64 {
        self.busy_timeout_ms
    }

    /// Параметризованное выполнение инструкции.
    pub fn execute(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<usize, DomainError> {
        self.transaction
            .execute(sql, parameters)
            .map_err(|error| map_error(&error, "не удалось выполнить запись в базу learning"))
    }

    /// Чтение одной строки одним параметризованным запросом.
    pub fn query_row<T>(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
        mapper: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, DomainError> {
        self.transaction
            .query_row(sql, parameters, mapper)
            .map_err(|error| map_error(&error, "не удалось прочитать строку базы learning"))
    }

    /// Подсчёт строк одним параметризованным запросом.
    pub fn count(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<usize, DomainError> {
        let value: i64 = self
            .transaction
            .query_row(sql, parameters, |row| row.get(0))
            .map_err(|error| map_error(&error, "не удалось посчитать строки базы learning"))?;
        Ok(usize::try_from(value.max(0)).unwrap_or(usize::MAX))
    }
}

/// Транзакция чтения.
pub type LearningRead<'a> = LearningTx<'a>;

/// Определяет режим журнала по доказанному типу файловой системы.
///
/// `WAL` включается только для известных локальных ФС. Сетевая или неизвестная
/// ФС получает честный fallback `DELETE`; предположение, что `WAL` работает
/// везде и даёт нескольких одновременных писателей, не делается.
#[must_use]
pub fn journal_mode_decision(database: &Path) -> JournalModeDecision {
    let probe = probe_directory(database);
    match filesystem_type(&probe) {
        Some(file_type) => {
            if let Some((_, name)) = NETWORK_FILESYSTEMS
                .iter()
                .find(|(magic, _)| *magic == file_type)
            {
                JournalModeDecision {
                    mode: "delete".to_owned(),
                    reason: format!(
                        "файловая система {name} (f_type {file_type:#x}) может быть сетевой: выбран DELETE"
                    ),
                }
            } else if let Some((_, name)) = LOCAL_FILESYSTEMS
                .iter()
                .find(|(magic, _)| *magic == file_type)
            {
                JournalModeDecision {
                    mode: "wal".to_owned(),
                    reason: format!(
                        "файловая система {name} (f_type {file_type:#x}) доказанно локальная: выбран WAL с busy_timeout"
                    ),
                }
            } else {
                JournalModeDecision {
                    mode: "delete".to_owned(),
                    reason: format!(
                        "тип файловой системы {file_type:#x} неизвестен: выбран консервативный DELETE"
                    ),
                }
            }
        }
        None => JournalModeDecision {
            mode: "delete".to_owned(),
            reason:
                "тип файловой системы определить не удалось (нет каталога или не поддерживается платформой): выбран консервативный DELETE"
                    .to_owned(),
        },
    }
}

/// Каталог, тип файловой системы которого проверяется.
fn probe_directory(database: &Path) -> PathBuf {
    if database.is_dir() {
        return database.to_path_buf();
    }
    match database.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(unix)]
fn filesystem_type(path: &Path) -> Option<i64> {
    match rustix::fs::statfs(path) {
        Ok(status) => {
            let value: i64 = status.f_type;
            Some(value)
        }
        Err(_) => None,
    }
}

#[cfg(not(unix))]
fn filesystem_type(_path: &Path) -> Option<i64> {
    None
}

fn map_open_error(options: &StoreOptions, error: &rusqlite::Error, context: &str) -> DomainError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            busy(error)
        }
        Some(rusqlite::ErrorCode::NotADatabase) => DomainError::with_details(
            ErrorCode::LearningCorrupt,
            format!(
                "Файл локальной базы learning не является базой SQLite: {error}; \
                 исходные данные не удаляются"
            ),
            crate::details! {
                "database" => options.display_path.clone(),
                "recovery" => recovery_paths(&options.display_path),
            },
        ),
        _ => DomainError::with_details(
            ErrorCode::LearningStorageUnavailable,
            format!("{context}: {error}"),
            crate::details! { "database" => options.display_path.clone() },
        ),
    }
}

/// Отображает ошибку SQLite в доменную ошибку, не маскируя блокировку успехом.
pub fn map_error(error: &rusqlite::Error, context: &str) -> DomainError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            busy(error)
        }
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt) => {
            DomainError::with_details(
                ErrorCode::LearningCorrupt,
                format!("{context}: {error}"),
                crate::details! {
                    "database" => DEFAULT_LEARNING_DATABASE,
                    "recovery" => recovery_paths(DEFAULT_LEARNING_DATABASE),
                },
            )
        }
        _ => DomainError::with_details(
            ErrorCode::LearningStorageUnavailable,
            format!("{context}: {error}"),
            crate::details! { "sqlite_error" => error.to_string() },
        ),
    }
}

fn busy(error: &rusqlite::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::LearningStorageBusy,
        format!(
            "Локальная база learning удерживается другой транзакцией дольше busy_timeout: {error}"
        ),
        crate::details! { "busy_timeout_ms" => DEFAULT_BUSY_TIMEOUT_MS },
    )
}

impl LearningStore {
    /// Доменная ошибка для недоступного хранилища.
    #[must_use]
    pub fn unavailable_error(&self, detail: &str) -> DomainError {
        unavailable(&self.options, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_local_filesystem_selects_wal_and_reports_reason() {
        let directory = temp_directory("wal-local");
        let decision = journal_mode_decision(&directory.join("state.sqlite"));
        assert_eq!(decision.mode, "wal");
        assert!(decision.reason.contains("локальная"));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn missing_directory_falls_back_to_delete_honestly() {
        let decision = journal_mode_decision(Path::new("/nonexistent-learning-probe/state.sqlite"));
        assert_eq!(decision.mode, "delete");
        assert!(decision.reason.contains("определить не удалось"));
    }

    #[test]
    fn filesystem_tables_never_contradict_each_other() {
        for (magic, name) in NETWORK_FILESYSTEMS {
            assert!(
                !LOCAL_FILESYSTEMS.iter().any(|(local, _)| local == magic),
                "{name} не может быть одновременно локальной и сетевой"
            );
        }
        for (magic, name) in LOCAL_FILESYSTEMS {
            assert!(
                !NETWORK_FILESYSTEMS
                    .iter()
                    .any(|(network, _)| network == magic),
                "{name} не может быть одновременно локальной и сетевой"
            );
        }
        // За одним f_type FUSE стоит и локальный virtiofs, и сетевой sshfs,
        // поэтому доказанной локальности нет: выбирается DELETE.
        assert!(
            NETWORK_FILESYSTEMS
                .iter()
                .any(|(magic, _)| *magic == 0x65735546)
        );
    }

    #[test]
    fn store_creates_schema_and_reports_status() {
        let directory = temp_directory("store-status");
        let options = StoreOptions::at(directory.join("state.sqlite"));
        let store = LearningStore::open(options).unwrap();
        assert_eq!(store.integrity_check().unwrap(), ());
        let status = store.status().unwrap();
        assert!(status.present);
        assert_eq!(status.user_version, LEARNING_SCHEMA_VERSION);
        assert_eq!(status.policy_version, LEARNING_POLICY_VERSION);
        assert!(status.fts5_available);
        assert_eq!(status.generation.revision, 0);
        assert!(!status.database_path.contains("/home/"));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn backup_uses_transactional_snapshot_and_refuses_overwrite() {
        let directory = temp_directory("store-backup");
        let store = LearningStore::open(StoreOptions::at(directory.join("state.sqlite"))).unwrap();
        let target = directory.join("snapshot.sqlite");
        store.backup_to(&target).unwrap();
        assert!(target.is_file());
        let error = store.backup_to(&target).unwrap_err();
        assert_eq!(error.code, ErrorCode::ReviewArtifactConflict);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn backup_rejects_a_non_utf8_destination_without_substituting_it() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let directory = temp_directory("store-backup-non-utf8");
        let store = LearningStore::open(StoreOptions::at(directory.join("state.sqlite"))).unwrap();
        // SQLite принимает путь только строкой UTF-8: подменять его нельзя, иначе
        // снимок оказался бы не там, где его ждут.
        let destination = directory.join(OsStr::from_bytes(b"snapshot-\xff.sqlite"));
        let error = store.backup_to(&destination).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(
            error.message.contains("UTF-8"),
            "ошибка обязана называть причину: {}",
            error.message
        );
        assert!(
            !directory.join("snapshot-\u{fffd}.sqlite").exists(),
            "подстановка не-UTF-8 пути запрещена"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn corrupt_file_is_reported_without_deleting_it() {
        let directory = temp_directory("store-corrupt");
        let database = directory.join("state.sqlite");
        std::fs::write(&database, b"not a sqlite database at all").unwrap();
        let error = LearningStore::open(StoreOptions::at(&database)).unwrap_err();
        assert!(
            matches!(
                error.code,
                ErrorCode::LearningCorrupt | ErrorCode::LearningStorageUnavailable
            ),
            "неожиданный код: {error:?}"
        );
        assert!(database.is_file(), "исходный файл не должен удаляться");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn closed_store_reports_unavailable_instead_of_creating_files() {
        let directory = temp_directory("store-absent");
        let mut options = StoreOptions::at(directory.join("state.sqlite"));
        options.create = false;
        let error = LearningStore::open(options).unwrap_err();
        assert_eq!(error.code, ErrorCode::LearningStorageUnavailable);
        assert!(!directory.join("state.sqlite").exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    pub(crate) fn temp_directory(label: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "anki-repo-learning-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        );
        path.push(unique);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
